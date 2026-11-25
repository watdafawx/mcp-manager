//! Notification-area icon of the daemon.
//!
//! [`menu`] and [`tooltip`] are pure (state in, labels out) and unit-tested. The Win32 part
//! (`win` module) is one thread with a hidden window and a message loop around raw
//! `Shell_NotifyIconW`; it needs no extra crates and holds a few hundred KB.

use std::sync::Arc;

use crate::update::Phase;

/// What the menu shows; filled from the daemon on every open.
#[derive(Debug, Clone, PartialEq)]
pub struct TraySnapshot {
    pub version: String,
    pub port: u16,
    pub servers: usize,
    pub enabled_servers: usize,
    pub running_instances: usize,
    pub autostart: bool,
    /// `None` when updating is switched off for this process.
    pub update: Option<UpdateView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateView {
    /// Newer version found, if any.
    pub latest: Option<String>,
    pub phase: Phase,
    pub waiting_for_idle: bool,
}

/// What a menu entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    Open,
    RestartAll,
    CheckUpdates,
    InstallUpdate,
    /// The new state of "Start with Windows".
    SetAutostart(bool),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Separator,
    Item {
        label: String,
        cmd: Option<TrayCmd>,
        enabled: bool,
        checked: bool,
        /// Shown bold: what a double click does.
        default: bool,
    },
}

impl Entry {
    fn item(label: impl Into<String>, cmd: TrayCmd) -> Self {
        Entry::Item {
            label: label.into(),
            cmd: Some(cmd),
            enabled: true,
            checked: false,
            default: false,
        }
    }
    fn info(label: impl Into<String>) -> Self {
        Entry::Item {
            label: label.into(),
            cmd: None,
            enabled: false,
            checked: false,
            default: false,
        }
    }
    fn disabled(mut self) -> Self {
        if let Entry::Item { enabled, .. } = &mut self {
            *enabled = false;
        }
        self
    }
    fn checked(mut self, on: bool) -> Self {
        if let Entry::Item { checked, .. } = &mut self {
            *checked = on;
        }
        self
    }
    fn bold(mut self) -> Self {
        if let Entry::Item { default, .. } = &mut self {
            *default = true;
        }
        self
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The first, disabled line: `Running · 3 servers · port 39494`.
pub fn status_line(s: &TraySnapshot) -> String {
    format!(
        "Running · {} · port {}",
        plural(s.servers, "server", "servers"),
        s.port
    )
}

/// Tooltip text (Windows cuts it at 127 characters).
pub fn tooltip(s: &TraySnapshot) -> String {
    let mut t = format!(
        "MCP Manager {} · {} ({} running) · port {}",
        s.version,
        plural(s.servers, "server", "servers"),
        s.running_instances,
        s.port
    );
    if let Some(v) = s.update.as_ref().and_then(|u| u.latest.as_deref()) {
        t.push_str(&format!(" · update {v} available"));
    }
    t.chars().take(127).collect()
}

/// The context menu for `s`.
pub fn menu(s: &TraySnapshot) -> Vec<Entry> {
    let mut m = vec![
        Entry::info(status_line(s)),
        Entry::Separator,
        Entry::item("Open MCP Manager", TrayCmd::Open).bold(),
        Entry::item("Restart all servers", TrayCmd::RestartAll),
        Entry::Separator,
    ];
    if let Some(u) = &s.update {
        let busy = matches!(u.phase, Phase::Downloading | Phase::Installing);
        match (&u.latest, busy) {
            (Some(v), true) => m.push(Entry::info(format!("Updating to {v}…"))),
            (Some(v), false) => {
                m.push(Entry::item(
                    format!("Install update {v}"),
                    TrayCmd::InstallUpdate,
                ));
                if u.waiting_for_idle {
                    m.push(Entry::info("Waiting for running tool calls to finish"));
                }
            }
            (None, _) => {}
        }
        m.push(if u.phase == Phase::Checking {
            Entry::info("Checking for updates…")
        } else {
            let e = Entry::item("Check for updates", TrayCmd::CheckUpdates);
            if busy { e.disabled() } else { e }
        });
        m.push(Entry::Separator);
    }
    m.push(
        Entry::item("Start with Windows", TrayCmd::SetAutostart(!s.autostart)).checked(s.autostart),
    );
    m.push(Entry::Separator);
    m.push(Entry::item("Quit MCP Manager", TrayCmd::Quit));
    m
}

/// What the tray asks of the daemon. Implemented by the daemon; every call must return quickly
/// (the message loop must not block): do the work on another thread.
pub trait TrayHost: Send + Sync + 'static {
    fn snapshot(&self) -> TraySnapshot;
    fn run(&self, cmd: TrayCmd);
}

/// Running tray; dropping it removes the icon.
pub struct TrayHandle {
    #[cfg(windows)]
    inner: win::Running,
}

impl TrayHandle {
    /// Starts the icon thread. `None` when this session has no desktop, or the icon cannot be
    /// created (the daemon runs fine without it).
    pub fn start(host: Arc<dyn TrayHost>) -> Option<TrayHandle> {
        #[cfg(windows)]
        {
            win::start(host).map(|inner| TrayHandle { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = host;
            None
        }
    }

    pub fn stop(self) {
        #[cfg(windows)]
        self.inner.stop();
    }
}

/// Brings an open MCP Manager window to the front. `false` = there is none.
pub fn focus_gui_window() -> bool {
    #[cfg(windows)]
    {
        win::focus_gui_window()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::thread::JoinHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
        Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use super::{Entry, TrayCmd, TrayHost, menu, tooltip};

    const WM_TRAY: u32 = WM_APP + 1;
    const TIMER_REFRESH: usize = 1;
    const REFRESH_MS: u32 = 5000;
    const ID_BASE: usize = 1000;
    const ICON_RESOURCE: usize = 1;
    const GUI_TITLE: &str = "MCP Manager";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Whether this process runs on a visible (interactive) window station.
    fn has_desktop() -> bool {
        // SAFETY: plain Win32 query into a stack struct of the right size.
        unsafe {
            let ws = GetProcessWindowStation();
            if ws.is_null() {
                return false;
            }
            let mut flags: USEROBJECTFLAGS = std::mem::zeroed();
            let mut needed = 0u32;
            let ok = GetUserObjectInformationW(
                ws,
                UOI_FLAGS,
                &mut flags as *mut _ as *mut c_void,
                std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                &mut needed,
            );
            ok != 0 && flags.dwFlags & WSF_VISIBLE as u32 != 0
        }
    }

    struct Ctx {
        host: Arc<dyn TrayHost>,
        added: bool,
        tip: String,
        taskbar_created: u32,
        icon: HICON,
    }

    pub struct Running {
        hwnd: Arc<AtomicIsize>,
        thread: JoinHandle<()>,
    }

    impl Running {
        pub fn stop(self) {
            let h = self.hwnd.load(Ordering::SeqCst);
            if h != 0 {
                // SAFETY: posting to a window of ours; a stale handle only makes this a no-op.
                unsafe {
                    PostMessageW(h as HWND, WM_CLOSE, 0, 0);
                }
            }
            let _ = self.thread.join();
        }
    }

    pub fn start(host: Arc<dyn TrayHost>) -> Option<Running> {
        if !has_desktop() {
            return None;
        }
        let hwnd = Arc::new(AtomicIsize::new(0));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
        let h2 = hwnd.clone();
        let thread = std::thread::Builder::new()
            .name("mcpm-tray".into())
            .spawn(move || thread_main(host, h2, ready_tx))
            .ok()?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(true) => Some(Running { hwnd, thread }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }

    fn load_icon() -> HICON {
        // SAFETY: icon resource 1 is embedded by the build script; falls back to the stock icon.
        unsafe {
            let inst = GetModuleHandleW(std::ptr::null());
            let size = GetSystemMetrics(SM_CXSMICON);
            let icon = LoadImageW(
                inst,
                ICON_RESOURCE as *const u16,
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            );
            if icon.is_null() {
                LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
            } else {
                icon as HICON
            }
        }
    }

    fn nid(hwnd: HWND, flags: u32, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
        // SAFETY: NOTIFYICONDATAW is plain data; all-zero is a valid empty value.
        let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = hwnd;
        d.uID = 1;
        d.uFlags = flags;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = icon;
        for (i, c) in tip.encode_utf16().take(d.szTip.len() - 1).enumerate() {
            d.szTip[i] = c;
        }
        d
    }

    fn add_icon(hwnd: HWND, ctx: &mut Ctx) {
        let d = nid(hwnd, NIF_MESSAGE | NIF_ICON | NIF_TIP, ctx.icon, &ctx.tip);
        // SAFETY: valid NOTIFYICONDATAW for our window.
        ctx.added = unsafe { Shell_NotifyIconW(NIM_ADD, &d) } != 0;
    }

    fn refresh(hwnd: HWND, ctx: &mut Ctx) {
        let tip = tooltip(&ctx.host.snapshot());
        if !ctx.added {
            ctx.tip = tip;
            add_icon(hwnd, ctx); // explorer may not have been up yet
        } else if tip != ctx.tip {
            ctx.tip = tip;
            let d = nid(hwnd, NIF_TIP, ctx.icon, &ctx.tip);
            // SAFETY: as above.
            unsafe {
                Shell_NotifyIconW(NIM_MODIFY, &d);
            }
        }
    }

    fn thread_main(
        host: Arc<dyn TrayHost>,
        out: Arc<AtomicIsize>,
        ready: std::sync::mpsc::Sender<bool>,
    ) {
        // SAFETY: the usual window-class / hidden-window / message-loop sequence; the context box
        // is owned by the window (GWLP_USERDATA) and freed in WM_NCDESTROY.
        unsafe {
            let inst = GetModuleHandleW(std::ptr::null());
            let class = wide("mcpm-tray");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: inst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&wc);
            let ctx = Box::into_raw(Box::new(Ctx {
                tip: tooltip(&host.snapshot()),
                host,
                added: false,
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                icon: load_icon(),
            }));
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("mcpm tray").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                inst,
                ctx as *const c_void,
            );
            if hwnd.is_null() {
                drop(Box::from_raw(ctx));
                let _ = ready.send(false);
                return;
            }
            out.store(hwnd as isize, Ordering::SeqCst);
            let _ = ready.send(true);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            out.store(0, Ordering::SeqCst);
        }
    }

    unsafe fn ctx_of<'a>(hwnd: HWND) -> Option<&'a mut Ctx> {
        // SAFETY: GWLP_USERDATA holds the Box<Ctx> set in WM_NCCREATE, until WM_NCDESTROY.
        unsafe {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
            p.as_mut()
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: standard window procedure; see `ctx_of`.
        unsafe {
            match msg {
                WM_NCCREATE => {
                    let cs = lparam as *const CREATESTRUCTW;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                }
                WM_CREATE => {
                    if let Some(ctx) = ctx_of(hwnd) {
                        add_icon(hwnd, ctx);
                    }
                    SetTimer(hwnd, TIMER_REFRESH, REFRESH_MS, None);
                    return 0;
                }
                WM_TIMER if wparam == TIMER_REFRESH => {
                    if let Some(ctx) = ctx_of(hwnd) {
                        refresh(hwnd, ctx);
                    }
                    return 0;
                }
                WM_TRAY => {
                    if let Some(ctx) = ctx_of(hwnd) {
                        match lparam as u32 {
                            WM_LBUTTONDBLCLK => ctx.host.run(TrayCmd::Open),
                            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd, ctx),
                            _ => {}
                        }
                    }
                    return 0;
                }
                WM_CLOSE => {
                    DestroyWindow(hwnd);
                    return 0;
                }
                WM_DESTROY => {
                    KillTimer(hwnd, TIMER_REFRESH);
                    if let Some(ctx) = ctx_of(hwnd)
                        && ctx.added
                    {
                        let d = nid(hwnd, 0, ctx.icon, "");
                        Shell_NotifyIconW(NIM_DELETE, &d);
                        ctx.added = false;
                    }
                    PostQuitMessage(0);
                    return 0;
                }
                WM_NCDESTROY => {
                    let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut Ctx;
                    if !p.is_null() {
                        drop(Box::from_raw(p));
                    }
                }
                _ => {
                    if let Some(ctx) = ctx_of(hwnd)
                        && msg == ctx.taskbar_created
                        && msg != 0
                    {
                        add_icon(hwnd, ctx); // explorer restarted
                        return 0;
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }

    unsafe fn show_menu(hwnd: HWND, ctx: &mut Ctx) {
        let snap = ctx.host.snapshot();
        let entries = menu(&snap);
        // SAFETY: menu handle is created, used and destroyed here.
        unsafe {
            let hmenu = CreatePopupMenu();
            if hmenu.is_null() {
                return;
            }
            for (i, e) in entries.iter().enumerate() {
                match e {
                    Entry::Separator => {
                        AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
                    }
                    Entry::Item {
                        label,
                        enabled,
                        checked,
                        default,
                        ..
                    } => {
                        let mut flags = MF_STRING;
                        if !enabled {
                            flags |= MF_GRAYED;
                        }
                        if *checked {
                            flags |= MF_CHECKED;
                        }
                        AppendMenuW(hmenu, flags, ID_BASE + i, wide(label).as_ptr());
                        if *default {
                            SetMenuDefaultItem(hmenu, (ID_BASE + i) as u32, 0);
                        }
                    }
                }
            }
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenu(
                hmenu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(hmenu);
            if chosen > 0
                && let Some(Entry::Item { cmd: Some(c), .. }) =
                    entries.get(chosen as usize - ID_BASE)
            {
                ctx.host.run(*c);
            }
        }
    }

    /// The top-level "MCP Manager" window owned by an mcpm process, brought to the front.
    pub fn focus_gui_window() -> bool {
        // SAFETY: Win32 queries and handle closes only.
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), wide(GUI_TITLE).as_ptr());
            if hwnd.is_null() || !owned_by_mcpm(hwnd) {
                return false;
            }
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
            true
        }
    }

    unsafe fn owned_by_mcpm(hwnd: HWND) -> bool {
        // SAFETY: handle opened with query rights and closed again.
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return false;
            }
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            if ok == 0 {
                return false;
            }
            let theirs = String::from_utf16_lossy(&buf[..len as usize]);
            let theirs = theirs
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or("")
                .to_lowercase();
            let ours = std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
                .unwrap_or_default();
            !ours.is_empty() && theirs == ours
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> TraySnapshot {
        TraySnapshot {
            version: "0.1.0".into(),
            port: 39494,
            servers: 3,
            enabled_servers: 2,
            running_instances: 1,
            autostart: false,
            update: Some(UpdateView {
                latest: None,
                phase: Phase::Idle,
                waiting_for_idle: false,
            }),
        }
    }

    fn labels(m: &[Entry]) -> Vec<String> {
        m.iter()
            .map(|e| match e {
                Entry::Separator => "-".to_string(),
                Entry::Item { label, .. } => label.clone(),
            })
            .collect()
    }

    fn item<'a>(m: &'a [Entry], label: &str) -> &'a Entry {
        m.iter()
            .find(|e| matches!(e, Entry::Item { label: l, .. } if l == label))
            .unwrap_or_else(|| panic!("no item {label}: {:?}", labels(m)))
    }

    #[test]
    fn plain_menu() {
        let m = menu(&snap());
        assert_eq!(
            labels(&m),
            [
                "Running · 3 servers · port 39494",
                "-",
                "Open MCP Manager",
                "Restart all servers",
                "-",
                "Check for updates",
                "-",
                "Start with Windows",
                "-",
                "Quit MCP Manager"
            ]
        );
        // status line is information only
        assert!(matches!(
            &m[0],
            Entry::Item {
                cmd: None,
                enabled: false,
                ..
            }
        ));
        // Open is the double-click default
        assert!(matches!(
            item(&m, "Open MCP Manager"),
            Entry::Item {
                default: true,
                cmd: Some(TrayCmd::Open),
                ..
            }
        ));
    }

    #[test]
    fn singular_server_count() {
        let mut s = snap();
        s.servers = 1;
        assert_eq!(status_line(&s), "Running · 1 server · port 39494");
        s.servers = 0;
        assert_eq!(status_line(&s), "Running · 0 servers · port 39494");
    }

    #[test]
    fn known_update_offers_an_install_entry() {
        let mut s = snap();
        s.update.as_mut().unwrap().latest = Some("0.2.0".into());
        let m = menu(&s);
        assert!(matches!(
            item(&m, "Install update 0.2.0"),
            Entry::Item {
                cmd: Some(TrayCmd::InstallUpdate),
                enabled: true,
                ..
            }
        ));
        assert!(tooltip(&s).contains("update 0.2.0 available"));
        // waiting for a running tool call to finish is explained
        s.update.as_mut().unwrap().waiting_for_idle = true;
        let m = menu(&s);
        assert!(
            labels(&m)
                .iter()
                .any(|l| l.starts_with("Waiting for running"))
        );
    }

    #[test]
    fn update_in_progress_blocks_further_actions() {
        let mut s = snap();
        {
            let u = s.update.as_mut().unwrap();
            u.latest = Some("0.2.0".into());
            u.phase = Phase::Downloading;
        }
        let m = menu(&s);
        assert!(matches!(
            item(&m, "Updating to 0.2.0…"),
            Entry::Item {
                cmd: None,
                enabled: false,
                ..
            }
        ));
        assert!(matches!(
            item(&m, "Check for updates"),
            Entry::Item { enabled: false, .. }
        ));
        assert!(!labels(&m).iter().any(|l| l.starts_with("Install update")));
        s.update.as_mut().unwrap().phase = Phase::Checking;
        assert!(labels(&menu(&s)).contains(&"Checking for updates…".to_string()));
    }

    #[test]
    fn autostart_checkbox_toggles_to_the_opposite() {
        let mut s = snap();
        let m = menu(&s);
        assert!(matches!(
            item(&m, "Start with Windows"),
            Entry::Item {
                checked: false,
                cmd: Some(TrayCmd::SetAutostart(true)),
                ..
            }
        ));
        s.autostart = true;
        let m = menu(&s);
        assert!(matches!(
            item(&m, "Start with Windows"),
            Entry::Item {
                checked: true,
                cmd: Some(TrayCmd::SetAutostart(false)),
                ..
            }
        ));
    }

    #[test]
    fn update_entries_are_absent_when_updating_is_off() {
        let mut s = snap();
        s.update = None;
        let l = labels(&menu(&s));
        assert!(!l.iter().any(|x| x.contains("update")));
        assert!(l.contains(&"Quit MCP Manager".to_string()));
    }

    #[test]
    fn tooltip_is_short_enough_for_windows() {
        let mut s = snap();
        s.version = "9".repeat(300);
        assert_eq!(tooltip(&s).chars().count(), 127);
        let t = tooltip(&snap());
        assert_eq!(t, "MCP Manager 0.1.0 · 3 servers (1 running) · port 39494");
    }
}
