//! The menu bar app (AppKit, through objc2). Everything here runs on the
//! main thread; a timer reads the supervisor's state twice a second.

#![allow(unsafe_code)]

use std::cell::{Cell, OnceCell};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuItem,
    NSRunningApplication, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{
    ns_string, NSBundle, NSNotification, NSObject, NSObjectProtocol, NSSize, NSString, NSTimer, NSURL,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::bundle::{self, Bundle, Tool};
use crate::probe;
use crate::supervisor::{self, State, Supervisor};

/// In the app's defaults: flux was opened in the browser once (for Set up
/// flux), and Open at Login was set up once.
const OPENED_KEY: &str = "OpenedFluxOnce";
const LOGIN_KEY: &str = "SetUpOpenAtLogin";

pub fn run() {
    let mtm = MainThreadMarker::new().expect("flux.app starts on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // A menu bar app: no Dock icon or menu (Info.plist's LSUIElement says so too).
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}

#[derive(Default)]
struct Ivars {
    bundle: OnceCell<Bundle>,
    home: OnceCell<PathBuf>,
    supervisor: OnceCell<Arc<Supervisor>>,
    status_item: OnceCell<Retained<NSStatusItem>>,
    status: OnceCell<Retained<NSMenuItem>>,
    open: OnceCell<Retained<NSMenuItem>>,
    start: OnceCell<Retained<NSMenuItem>>,
    restart: OnceCell<Retained<NSMenuItem>>,
    login: OnceCell<Retained<NSMenuItem>>,
    tools: OnceCell<Retained<NSMenuItem>>,
    shown: Cell<Option<State>>,
    relaunching: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate has no
    // Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FluxAppDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.launched();
        }

        /// Quit (the menu, logout, shutdown): flux stops first.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            if let Some(sup) = self.ivars().supervisor.get() {
                sup.stop(supervisor::STOP_WITHIN);
            }
            NSApplicationTerminateReply::TerminateNow
        }
    }

    impl Delegate {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.refresh();
        }

        #[unsafe(method(openFlux:))]
        fn open_flux(&self, _sender: Option<&AnyObject>) {
            open_url(&self.url());
        }

        #[unsafe(method(startFlux:))]
        fn start_flux(&self, _sender: Option<&AnyObject>) {
            if let Some(sup) = self.ivars().supervisor.get() {
                sup.ask_start();
            }
        }

        #[unsafe(method(restartFlux:))]
        fn restart_flux(&self, _sender: Option<&AnyObject>) {
            if let Some(sup) = self.ivars().supervisor.get() {
                sup.restart();
            }
        }

        #[unsafe(method(editSettings:))]
        fn edit_settings(&self, _sender: Option<&AnyObject>) {
            if let Some(home) = self.ivars().home.get() {
                // In a text editor (TextEdit, unless the person chose another).
                let _ = Command::new("/usr/bin/open")
                    .arg("-t")
                    .arg(bundle::data_dir(home).join("flux.toml"))
                    .spawn();
            }
        }

        #[unsafe(method(toggleLogin:))]
        fn toggle_login(&self, _sender: Option<&AnyObject>) {
            let on = login_status() == SMAppServiceStatus::Enabled;
            if let Err(why) = set_login(!on) {
                self.alert("Open at Login didn't change", &why);
            }
            self.refresh_login();
        }

        #[unsafe(method(installTools:))]
        fn install_tools(&self, _sender: Option<&AnyObject>) {
            self.link_tools();
            self.refresh_tools();
        }

        #[unsafe(method(showLog:))]
        fn show_log(&self, _sender: Option<&AnyObject>) {
            if let Some(home) = self.ivars().home.get() {
                open_file(&bundle::log_file(home));
            }
        }

        #[unsafe(method(about:))]
        fn about(&self, _sender: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            activate(&app);
            app.orderFrontStandardAboutPanel(None);
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn launched(&self) {
        let mtm = self.mtm();
        let app = NSApplication::sharedApplication(mtm);
        let exe = std::env::current_exe().map(|e| e.canonicalize().unwrap_or(e));
        let (Some(bundle), Some(home)) = (exe.ok().as_deref().and_then(Bundle::of_exe), home()) else {
            self.alert("flux can't start", "flux.app is incomplete: download it again.");
            app.terminate(None);
            return;
        };
        let _ = self.ivars().home.set(home.clone());
        let _ = self.ivars().bundle.set(bundle.clone());

        // Opened again while it runs: show flux.
        if others_running() {
            open_url(
                &probe::read_server_file(&bundle::data_dir(&home))
                    .map(|f| f.url)
                    .unwrap_or_else(|| probe::DEFAULT_URL.to_string()),
            );
            app.terminate(None);
            return;
        }
        if bundle.needs_moving() {
            self.offer_to_move(&bundle);
            return;
        }

        let data_dir = bundle::data_dir(&home);
        let log = bundle::log_file(&home);
        let _ = bundle::rotate_log(&log, bundle::LOG_LIMIT);
        if let Err(e) = bundle::write_settings(&bundle.resource("flux.toml"), &data_dir) {
            eprintln!(
                "flux.app: couldn't write {}: {e}",
                data_dir.join("flux.toml").display()
            );
        }
        let sup = Supervisor::new(supervisor::Config {
            server: bundle.server(),
            data_dir,
            log,
            default_url: probe::DEFAULT_URL.to_string(),
        });
        sup.spawn();
        let _ = self.ivars().supervisor.set(sup);

        self.build_menu(mtm);
        // A server: on from the start, unless switched off since.
        if !default_bool(LOGIN_KEY) {
            set_default_bool(LOGIN_KEY, true);
            if login_status() != SMAppServiceStatus::Enabled {
                let _ = set_login(true);
            }
        }
        self.refresh();
        let target: &AnyObject = self;
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.5,
                target,
                sel!(tick:),
                None,
                true,
            );
        }
    }

    fn build_menu(&self, mtm: MainThreadMarker) {
        let ivars = self.ivars();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let status = self.item(&menu, "Starting flux…", None, "");
        status.setEnabled(false);
        let _ = ivars
            .open
            .set(self.item(&menu, "Open flux", Some(sel!(openFlux:)), "o"));
        let start = self.item(&menu, "Start flux", Some(sel!(startFlux:)), "");
        start.setHidden(true);
        let _ = ivars
            .restart
            .set(self.item(&menu, "Restart flux", Some(sel!(restartFlux:)), ""));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let _ = ivars
            .login
            .set(self.item(&menu, "Open at Login", Some(sel!(toggleLogin:)), ""));
        self.item(&menu, "Edit Settings…", Some(sel!(editSettings:)), ",");
        let _ = ivars.tools.set(self.item(
            &menu,
            "Install Command Line Tools…",
            Some(sel!(installTools:)),
            "",
        ));
        self.item(&menu, "Show Log", Some(sel!(showLog:)), "");
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.item(&menu, "About flux", Some(sel!(about:)), "");
        let quit = self.item(&menu, "Quit flux", Some(sel!(terminate:)), "q");
        let app = NSApplication::sharedApplication(mtm);
        let app: &AnyObject = &app;
        unsafe { quit.setTarget(Some(app)) };
        let _ = ivars.status.set(status);
        let _ = ivars.start.set(start);

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            match NSImage::imageNamed(ns_string!("StatusIcon")) {
                Some(image) => {
                    image.setTemplate(true);
                    image.setSize(NSSize::new(18.0, 18.0));
                    button.setImage(Some(&image));
                }
                None => button.setTitle(ns_string!("flux")),
            }
            button.setToolTip(Some(ns_string!("flux")));
        }
        item.setMenu(Some(&menu));
        let _ = ivars.status_item.set(item);
    }

    fn item(&self, menu: &NSMenu, title: &str, action: Option<Sel>, key: &str) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(title),
                action,
                &NSString::from_str(key),
            )
        };
        let target: &AnyObject = self;
        unsafe { item.setTarget(Some(target)) };
        menu.addItem(&item);
        item
    }

    /// Where flux answers, or would.
    fn url(&self) -> String {
        match self.ivars().supervisor.get().map(|s| s.state()) {
            Some(State::Running { url, .. } | State::Elsewhere { url }) => url,
            _ => probe::DEFAULT_URL.to_string(),
        }
    }

    fn refresh(&self) {
        let ivars = self.ivars();
        let Some(sup) = ivars.supervisor.get() else {
            return;
        };
        let state = sup.state();
        if state == State::Relaunch {
            self.relaunch();
            return;
        }
        let shown = ivars.shown.take();
        if shown.as_ref() != Some(&state) {
            self.show(&state);
            if matches!(state, State::Running { .. }) && !default_bool(OPENED_KEY) {
                set_default_bool(OPENED_KEY, true);
                open_url(&self.url());
            }
        }
        ivars.shown.set(Some(state));
        self.refresh_login();
        self.refresh_tools();
    }

    fn show(&self, state: &State) {
        let ivars = self.ivars();
        let (Some(status), Some(open), Some(start)) =
            (ivars.status.get(), ivars.open.get(), ivars.start.get())
        else {
            return;
        };
        let (text, up) = match state {
            State::Starting => ("Starting flux…".to_string(), false),
            State::Running { version, .. } => (format!("flux {version} is running"), true),
            State::Elsewhere { .. } => ("flux is running (not started by this app)".to_string(), true),
            State::Stopped { why } => (why.clone(), false),
            State::Relaunch | State::Quit => ("Stopping flux…".to_string(), false),
        };
        status.setTitle(&NSString::from_str(&text));
        open.setEnabled(up);
        start.setHidden(!matches!(state, State::Stopped { .. }));
        if let Some(restart) = ivars.restart.get() {
            restart.setHidden(!matches!(state, State::Running { .. } | State::Starting));
        }
        if let Some(button) = ivars.status_item.get().and_then(|i| i.button(self.mtm())) {
            button.setAppearsDisabled(!up);
            button.setToolTip(Some(&NSString::from_str(&text)));
        }
    }

    fn refresh_login(&self) {
        if let Some(item) = self.ivars().login.get() {
            item.setState(match login_status() {
                SMAppServiceStatus::Enabled => NSControlStateValueOn,
                _ => NSControlStateValueOff,
            });
        }
    }

    fn refresh_tools(&self) {
        let ivars = self.ivars();
        let Some(item) = ivars.tools.get() else {
            return;
        };
        let ours = bundle::TOOLS
            .iter()
            .all(|t| bundle::tool_at(&Path::new(bundle::TOOLS_DIR).join(t)) == Tool::App);
        item.setState(if ours {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }

    /// Links `flux` and `flux-server` from /usr/local/bin, asking for an
    /// administrator's password when that folder needs one.
    fn link_tools(&self) {
        let Some(bundle) = self.ivars().bundle.get() else {
            return;
        };
        let others: Vec<String> = bundle::TOOLS
            .iter()
            .map(|t| format!("{}/{t}", bundle::TOOLS_DIR))
            .filter(|p| bundle::tool_at(Path::new(p)) == Tool::Other)
            .collect();
        if !others.is_empty()
            && !self.confirm(
                "Replace the flux commands there?",
                &format!(
                    "{} {} another flux's (Homebrew's, or a release package's). flux.app's commands \
                     take their place.",
                    others.join(" and "),
                    if others.len() == 1 { "is" } else { "are" }
                ),
                "Replace",
            )
        {
            return;
        }
        let cmd = bundle::link_tools_command(bundle, bundle::TOOLS_DIR);
        let as_user = Command::new("/bin/sh")
            .args(["-c", &cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if as_user {
            return self.tools_linked();
        }
        let script = format!(
            "do shell script {} with administrator privileges",
            bundle::applescript_quote(&cmd)
        );
        match Command::new("/usr/bin/osascript").args(["-e", &script]).output() {
            Ok(out) if out.status.success() => self.tools_linked(),
            // -128: the person cancelled the password prompt.
            Ok(out) if String::from_utf8_lossy(&out.stderr).contains("(-128)") => {}
            Ok(out) => self.alert(
                "The command line tools weren't installed",
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => self.alert("The command line tools weren't installed", &e.to_string()),
        }
    }

    fn tools_linked(&self) {
        self.alert(
            "The command line tools are installed",
            "flux and flux-server in /usr/local/bin run flux.app's: `flux --help` for you and your \
             agents, `flux-server owner-link` to make another device an owner.",
        );
    }

    /// Asks to move flux.app to Applications, where it can update itself,
    /// and opens it from there; or quits.
    fn offer_to_move(&self, bundle: &Bundle) {
        let app = NSApplication::sharedApplication(self.mtm());
        if self.confirm(
            "Move flux to the Applications folder?",
            "flux runs from Applications, where it can keep itself up to date. It can't run from its \
             disk image.",
            "Move to Applications",
        ) {
            match move_to_applications(bundle) {
                Ok(moved) => {
                    let _ = Command::new("/usr/bin/open").arg(&moved).spawn();
                }
                Err(why) => self.alert("flux couldn't move itself", &why),
            }
        }
        app.terminate(None);
    }

    /// flux asked to start again after an update: open the bundle (new now)
    /// once this process has ended.
    fn relaunch(&self) {
        let ivars = self.ivars();
        if ivars.relaunching.replace(true) {
            return;
        }
        if let Some(bundle) = ivars.bundle.get() {
            let script =
                "while /bin/kill -0 \"$0\" 2> /dev/null; do /bin/sleep 0.2; done; exec /usr/bin/open \"$1\"";
            let _ = Command::new("/bin/sh")
                .args(["-c", script])
                .arg(std::process::id().to_string())
                .arg(&bundle.root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        NSApplication::sharedApplication(self.mtm()).terminate(None);
    }

    fn alert(&self, title: &str, text: &str) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        activate(&NSApplication::sharedApplication(self.mtm()));
        alert.runModal();
    }

    /// True when the person chose `yes`.
    fn confirm(&self, title: &str, text: &str, yes: &str) -> bool {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        alert.addButtonWithTitle(&NSString::from_str(yes));
        alert.addButtonWithTitle(ns_string!("Cancel"));
        activate(&NSApplication::sharedApplication(self.mtm()));
        alert.runModal() == NSAlertFirstButtonReturn
    }
}

#[allow(deprecated)]
fn activate(app: &NSApplication) {
    // `activate` is macOS 14 and later; flux.app runs on 13.
    app.activateIgnoringOtherApps(true);
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Another flux.app of this bundle identifier runs for this person.
fn others_running() -> bool {
    let Some(id) = NSBundle::mainBundle().bundleIdentifier() else {
        return false;
    };
    let me = std::process::id() as i32;
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&id)
        .iter()
        .any(|a| a.processIdentifier() != me)
}

fn open_url(url: &str) {
    if let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) {
        NSWorkspace::sharedWorkspace().openURL(&u);
    }
}

fn open_file(path: &Path) {
    let u = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSWorkspace::sharedWorkspace().openURL(&u);
}

fn default_bool(key: &str) -> bool {
    let d = objc2_foundation::NSUserDefaults::standardUserDefaults();
    d.boolForKey(&NSString::from_str(key))
}

fn set_default_bool(key: &str, value: bool) {
    let d = objc2_foundation::NSUserDefaults::standardUserDefaults();
    d.setBool_forKey(value, &NSString::from_str(key));
}

fn login_status() -> SMAppServiceStatus {
    unsafe { SMAppService::mainAppService().status() }
}

/// Registers flux.app as a login item, or unregisters it. When macOS wants
/// the person to allow it, opens Login Items in System Settings.
fn set_login(on: bool) -> Result<(), String> {
    let svc = unsafe { SMAppService::mainAppService() };
    let done = if on {
        unsafe { svc.registerAndReturnError() }
    } else {
        unsafe { svc.unregisterAndReturnError() }
    };
    done.map_err(|e| e.localizedDescription().to_string())?;
    if unsafe { svc.status() } == SMAppServiceStatus::RequiresApproval {
        unsafe { SMAppService::openSystemSettingsLoginItems() };
    }
    Ok(())
}

/// Copies flux.app to /Applications (in place of one there), without the
/// download's quarantine, which the person already cleared by opening it.
fn move_to_applications(bundle: &Bundle) -> Result<PathBuf, String> {
    let dest = PathBuf::from("/Applications/flux.app");
    let staged = PathBuf::from("/Applications/.flux-move.app");
    let run = |cmd: &mut Command| -> Result<(), String> {
        let out = cmd.output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    };
    let _ = std::fs::remove_dir_all(&staged);
    run(Command::new("/usr/bin/ditto").arg(&bundle.root).arg(&staged)).map_err(|e| {
        format!("Copying it to /Applications failed ({e}). Drag flux from its disk image to Applications instead.")
    })?;
    let _ = run(Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&staged));
    if dest.exists() {
        let aside = PathBuf::from(format!("/Applications/.flux-replaced-{}.app", std::process::id()));
        std::fs::rename(&dest, &aside)
            .map_err(|e| format!("Replacing /Applications/flux.app failed: {e}"))?;
        let _ = std::fs::remove_dir_all(&aside);
    }
    std::fs::rename(&staged, &dest).map_err(|e| format!("Moving flux into /Applications failed: {e}"))?;
    Ok(dest)
}
