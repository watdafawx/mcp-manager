//! Small line diff for the snapshot viewer (stored bytes vs the current file).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Same,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub tag: Tag,
    pub text: String,
}

/// LCS table cells allowed before we give up and show the middle as one replaced block.
const MAX_CELLS: usize = 4_000_000;

/// `old` -> `new`. Common prefix/suffix are trimmed first so big files with a small edit stay cheap.
pub fn diff_lines(old: &str, new: &str) -> Vec<Line> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);

    let mut out: Vec<Line> = a[..pre].iter().map(|t| line(Tag::Same, t)).collect();
    if ma.len().saturating_mul(mb.len()) > MAX_CELLS {
        out.extend(ma.iter().map(|t| line(Tag::Del, t)));
        out.extend(mb.iter().map(|t| line(Tag::Add, t)));
    } else {
        out.extend(lcs_diff(ma, mb));
    }
    out.extend(a[a.len() - suf..].iter().map(|t| line(Tag::Same, t)));
    out
}

fn line(tag: Tag, t: &str) -> Line {
    Line { tag, text: t.to_string() }
}

fn lcs_diff(a: &[&str], b: &[&str]) -> Vec<Line> {
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if a[i] == b[j] {
                t[(i + 1) * w + j + 1] + 1
            } else {
                t[(i + 1) * w + j].max(t[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n + m);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(line(Tag::Same, a[i]));
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            out.push(line(Tag::Del, a[i]));
            i += 1;
        } else {
            out.push(line(Tag::Add, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|t| line(Tag::Del, t)));
    out.extend(b[j..].iter().map(|t| line(Tag::Add, t)));
    out
}

/// True when every line is `Same`.
pub fn is_identical(d: &[Line]) -> bool {
    d.iter().all(|l| l.tag == Tag::Same)
}

/// A displayed row: a diff line, or a run of unchanged lines folded away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Line(Line),
    Fold(usize),
}

/// Keeps `ctx` unchanged lines around each change and folds the rest into `Row::Fold`.
/// An identical file folds into a single row.
pub fn fold_context(lines: &[Line], ctx: usize) -> Vec<Row> {
    let n = lines.len();
    let mut keep = vec![false; n];
    for (i, l) in lines.iter().enumerate() {
        if l.tag != Tag::Same {
            let (from, to) = (i.saturating_sub(ctx), (i + ctx + 1).min(n));
            keep[from..to].fill(true);
        }
    }
    let mut out = Vec::new();
    let mut run = 0;
    for (i, l) in lines.iter().enumerate() {
        if keep[i] {
            if run > 0 {
                out.push(Row::Fold(run));
                run = 0;
            }
            out.push(Row::Line(l.clone()));
        } else {
            run += 1;
        }
    }
    if run > 0 {
        out.push(Row::Fold(run));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(d: &[Line]) -> String {
        d.iter()
            .map(|l| match l.tag {
                Tag::Same => '=',
                Tag::Add => '+',
                Tag::Del => '-',
            })
            .collect()
    }

    #[test]
    fn identical_and_edit() {
        assert!(is_identical(&diff_lines("a\nb", "a\nb")));
        let d = diff_lines("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(tags(&d), "=-+=");
    }

    #[test]
    fn insert_delete_and_empty() {
        assert_eq!(tags(&diff_lines("", "a\nb")), "++");
        assert_eq!(tags(&diff_lines("a\nb", "")), "--");
        assert_eq!(tags(&diff_lines("a\nc", "a\nb\nc")), "=+=");
    }

    #[test]
    fn folds_unchanged_runs() {
        let old: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "changed\n");
        let rows = fold_context(&diff_lines(&old, &new), 2);
        assert_eq!(rows.first(), Some(&Row::Fold(8)));
        assert_eq!(rows.last(), Some(&Row::Fold(7)));
        assert_eq!(rows.len(), 1 + 2 + 2 + 2 + 1);
        assert_eq!(fold_context(&diff_lines("a\nb", "a\nb"), 3), vec![Row::Fold(2)]);
    }

    #[test]
    fn huge_middle_falls_back() {
        let a: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let d = diff_lines(&a, &b);
        assert_eq!(d.len(), 6000);
    }
}
