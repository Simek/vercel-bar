#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::{Context, Result};
use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength,
};
use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSRange, NSRect, NSSize, NSString};
use keyring::Entry;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use once_cell::sync::OnceCell;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::{process::Command, sync::{Mutex, atomic::{AtomicUsize, Ordering}}, time::{Duration, Instant}};

use crate::vercel::{AppSnapshot, ProjectSnapshot, TeamSnapshot, VercelClient};

const KEYRING_SERVICE: &str = "dev-tree";
const KEYRING_ACCOUNT: &str = "vercel-token";
const NS_ALERT_FIRST_BUTTON_RETURN: i64 = 1000;
const NS_ALERT_THIRD_BUTTON_RETURN: i64 = 1002;
const CACHE_TTL: Duration = Duration::from_secs(300);
static SNAPSHOT_CACHE: OnceCell<Mutex<Option<(Instant, AppSnapshot)>>> = OnceCell::new();
static STATUS_ITEM: AtomicUsize = AtomicUsize::new(0);
static APP_MENU: AtomicUsize = AtomicUsize::new(0);

struct AppLifetime {
    _controller: id,
    _status_item: id,
}

pub fn run() {
    unsafe {
        dotenvy::dotenv().ok();
        let _pool = NSAutoreleasePool::new(nil);
        let app = NSApp();
        app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);

        let controller = new_controller();
        let status_bar = NSStatusBar::systemStatusBar(nil);
        let status_item = status_bar.statusItemWithLength_(NSVariableStatusItemLength);
        STATUS_ITEM.store(status_item as usize, Ordering::Relaxed);
        let button: id = msg_send![status_item, button];
        let _: () = msg_send![button, setTitle: ns_string("▲")];

        let menu = NSMenu::new(nil);
        let _: () = msg_send![menu, setAutoenablesItems: NO];
        let _: () = msg_send![menu, setDelegate: controller];
        populate_loading_menu(menu, controller);
        let _: () = msg_send![status_item, setMenu: menu];
        APP_MENU.store(menu as usize, Ordering::Relaxed);
        let _: id = msg_send![class!(NSTimer), scheduledTimerWithTimeInterval: 300.0f64 target: controller selector: sel!(autoRefresh:) userInfo: nil repeats: YES];

        let lifetime = Box::new(AppLifetime {
            _controller: controller,
            _status_item: status_item,
        });
        Box::leak(lifetime);

        app.run();
    }
}

unsafe fn populate_loading_menu(menu: id, controller: id) {
    clear_menu(menu);
    add_static_item(menu, "Loading…");
    add_separator(menu);
    add_action_item(menu, "Settings…", sel!(openSettings:), controller);
    add_action_item(menu, "Quit", sel!(quitApp:), controller);
}

extern "C" fn menu_will_open(this: &Object, _: Sel, menu: id) {
    if cached_snapshot().is_some() {
        return;
    }
    unsafe {
        let _: id = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.01f64
            target: this as *const _ as id
            selector: sel!(loadMenu:)
            userInfo: menu
            repeats: NO
        ];
    }
}

extern "C" fn load_menu(this: &Object, _: Sel, timer: id) {
    unsafe {
        let menu: id = msg_send![timer, userInfo];
        rebuild_menu(menu, this as *const _ as id);
    }
}

extern "C" fn refresh_menu(this: &Object, _: Sel, sender: id) {
    unsafe {
        let menu: id = msg_send![sender, menu];
        if let Some(cache) = SNAPSHOT_CACHE.get() {
            *cache.lock().unwrap() = None;
        }
        set_status_title("♻");
        let _: id = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.01f64
            target: this as *const _ as id
            selector: sel!(loadMenu:)
            userInfo: menu
            repeats: NO
        ];
    }
}

extern "C" fn open_settings(this: &Object, _: Sel, _: id) {
    unsafe {
        match show_settings_modal() {
            Ok(()) => {
                let menu = APP_MENU.load(Ordering::Relaxed) as id;
                if menu != nil {
                    rebuild_menu(menu, this as *const _ as id);
                }
            }
            Err(err) => show_error_alert("Failed to update Vercel token", &err.to_string()),
        }
    }
}

extern "C" fn open_represented_url(_: &Object, _: Sel, sender: id) {
    unsafe {
        let represented: id = msg_send![sender, representedObject];
        if represented == nil {
            return;
        }

        if let Err(err) = open_url(&ns_string_to_rust(represented)) {
            show_error_alert("Failed to open link", &err.to_string());
        }
    }
}

extern "C" fn quit_app(_: &Object, _: Sel, _: id) {
    unsafe {
        let app = NSApp();
        let _: () = msg_send![app, terminate: nil];
    }
}

unsafe fn rebuild_menu(menu: id, controller: id) {
    let _: () = msg_send![NSApp(), updateWindows];
    clear_menu(menu);
    add_static_item(menu, "Refreshing Vercel projects…");

    match load_token() {
        Ok(Some(token)) => {
            set_status_title("♻");
            if let Some(snapshot) = cached_snapshot() {
                populate_snapshot_menu(menu, controller, snapshot);
            } else {
                match VercelClient::new(token).and_then(|client| client.fetch_snapshot()) {
                    Ok(snapshot) => {
                        cache_snapshot(snapshot.clone());
                        populate_snapshot_menu(menu, controller, snapshot);
                    }
                Err(err) => populate_error_menu(
                    menu,
                    controller,
                    &format!("Failed to load Vercel data: {err}"),
                    ),
                }
            }
        }
        Ok(None) => populate_missing_token_menu(menu, controller),
        Err(err) => populate_error_menu(
            menu,
            controller,
            &format!("Failed to read saved token: {err}"),
        ),
    }
    if load_token().ok().flatten().is_some() {
        set_status_title("▲");
    }
    let _: () = msg_send![menu, update];
}

extern "C" fn auto_refresh(this: &Object, _: Sel, _: id) {
    if cached_snapshot().is_some() { return; }
    unsafe {
        let menu = APP_MENU.load(Ordering::Relaxed) as id;
        if menu != nil { rebuild_menu(menu, this as *const _ as id); }
    }
}

unsafe fn set_status_title(title: &str) {
    let item = STATUS_ITEM.load(Ordering::Relaxed) as id;
    if item != nil {
        let button: id = msg_send![item, button];
        let _: () = msg_send![button, setTitle: ns_string(title)];
        let _: () = msg_send![button, displayIfNeeded];
        let _: () = msg_send![NSApp(), updateWindows];
    }
}

unsafe fn populate_snapshot_menu(menu: id, controller: id, snapshot: AppSnapshot) {
    clear_menu(menu);

    if snapshot.teams.is_empty() {
        add_static_item(menu, "No projects found for this Vercel account.");
    } else {
        for team in snapshot.teams {
            add_team_menu(menu, controller, &team);
        }
    }

    add_footer_items(menu, controller);
}

unsafe fn populate_missing_token_menu(menu: id, controller: id) {
    clear_menu(menu);
    add_static_item(
        menu,
        "Add a Vercel token in Settings.",
    );
    add_footer_items(menu, controller);
}

unsafe fn populate_error_menu(menu: id, controller: id, message: &str) {
    clear_menu(menu);
    add_static_item(menu, message);
    add_footer_items(menu, controller);
}

unsafe fn add_team_menu(menu: id, controller: id, team: &TeamSnapshot) {
    add_separator(menu);
    add_static_item(menu, &format!("{} ({})", team.name, team.projects.len()));
    for project in &team.projects {
        add_project_menu(menu, controller, project);
    }
}

unsafe fn add_project_menu(menu: id, controller: id, project: &ProjectSnapshot) {
    let title = format!("{} {}", project.latest_status.indicator(), project.name);
    let project_item = make_menu_item(&title);
    let _: () = msg_send![
        project_item,
        setAttributedTitle: colored_title(&title, &project.latest_status)
    ];
    let project_menu = NSMenu::new(nil);
    let _: () = msg_send![project_menu, setAutoenablesItems: NO];

    add_static_item(project_menu, &project.latest_status_label);
    add_separator(project_menu);

    if let Some(url) = &project.latest_deployment_url {
        add_url_item(project_menu, "Open latest deployment", url, controller);
    }

    unsafe fn colored_title(title: &str, status: &crate::vercel::DeploymentStatus) -> id {
        let color: id = match status {
            crate::vercel::DeploymentStatus::Healthy => msg_send![class!(NSColor), systemGreenColor],
            crate::vercel::DeploymentStatus::Failed => msg_send![class!(NSColor), systemRedColor],
            crate::vercel::DeploymentStatus::Pending => msg_send![class!(NSColor), systemOrangeColor],
            crate::vercel::DeploymentStatus::Missing => msg_send![class!(NSColor), secondaryLabelColor],
        };
        let attributed: id = msg_send![
            class!(NSMutableAttributedString),
            alloc
        ];
        let attributed: id = msg_send![
            attributed,
            initWithString: ns_string(title)
        ];
        let _: () = msg_send![
            attributed,
            addAttribute: ns_string("NSColor")
            value: color
            range: NSRange::new(0, 1)
        ];
        attributed
    }

    add_url_item(
        project_menu,
        "Open website logs",
        &project.logs_url,
        controller,
    );
    add_url_item(
        project_menu,
        "Open project dashboard",
        &project.dashboard_url,
        controller,
    );
    if let Some(url) = &project.production_url {
        add_url_item(project_menu, "Open production site", url, controller);
    }

    let _: () = msg_send![project_item, setSubmenu: project_menu];
    let _: () = msg_send![menu, addItem: project_item];
}

fn cache_snapshot(snapshot: AppSnapshot) {
    let cache = SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    *cache.lock().unwrap() = Some((Instant::now(), snapshot));
}

fn clear_snapshot_cache() {
    if let Some(cache) = SNAPSHOT_CACHE.get() {
        *cache.lock().unwrap() = None;
    }
}

fn cached_snapshot() -> Option<AppSnapshot> {
    let cache = SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    let guard = cache.lock().unwrap();
    guard.as_ref().filter(|(time, _)| time.elapsed() < CACHE_TTL).map(|(_, snapshot)| snapshot.clone())
}

unsafe fn add_footer_items(menu: id, controller: id) {
    add_separator(menu);
    add_action_item(menu, "Refresh", sel!(refreshMenu:), controller);
    add_action_item(menu, "Settings…", sel!(openSettings:), controller);
    add_action_item(menu, "Quit", sel!(quitApp:), controller);
}

unsafe fn clear_menu(menu: id) {
    loop {
        let count: isize = msg_send![menu, numberOfItems];
        if count <= 0 {
            break;
        }

        let _: () = msg_send![menu, removeItemAtIndex: 0isize];
    }
}

unsafe fn add_separator(menu: id) {
    let separator: id = msg_send![class!(NSMenuItem), separatorItem];
    let _: () = msg_send![menu, addItem: separator];
}

unsafe fn add_static_item(menu: id, title: &str) {
    let item = make_menu_item(title);
    let _: () = msg_send![item, setEnabled: NO];
    let _: () = msg_send![menu, addItem: item];
}

unsafe fn add_action_item(menu: id, title: &str, action: Sel, controller: id) {
    let item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
        ns_string(title),
        action,
        ns_string(""),
    );
    let _: () = msg_send![item, setTarget: controller];
    let _: () = msg_send![menu, addItem: item];
}

unsafe fn add_url_item(menu: id, title: &str, url: &str, controller: id) {
    let item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
        ns_string(title),
        sel!(openRepresentedUrl:),
        ns_string(""),
    );
    let _: () = msg_send![item, setTarget: controller];
    let _: () = msg_send![item, setRepresentedObject: ns_string(url)];
    let _: () = msg_send![menu, addItem: item];
}

unsafe fn make_menu_item(title: &str) -> id {
    NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
        ns_string(title),
        sel!(noop:),
        ns_string(""),
    )
}

fn show_settings_modal() -> Result<()> {
    unsafe {
        let app = NSApp();
        let _: () = msg_send![app, activateIgnoringOtherApps: YES];

        let alert: id = msg_send![class!(NSAlert), new];
        let _: () = msg_send![alert, setMessageText: ns_string("Vercel token")];
        let _: () = msg_send![alert, setInformativeText: ns_string(
            "Enter a Vercel token to load teams and projects in the Vercel Bar menu bar app.",
        )];
        let _: id = msg_send![alert, addButtonWithTitle: ns_string("Save")];
        let _: id = msg_send![alert, addButtonWithTitle: ns_string("Cancel")];
        let _: id = msg_send![alert, addButtonWithTitle: ns_string("Clear Token")];

        let text_field: id = msg_send![class!(NSSecureTextField), alloc];
        let text_field: id = msg_send![text_field, initWithFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(0.0, 0.0),
            NSSize::new(360.0, 24.0),
        )];
        let _: () = msg_send![text_field, setEditable: YES];
        let _: () = msg_send![text_field, setSelectable: YES];
        if let Some(token) = load_token()? {
            let _: () = msg_send![text_field, setStringValue: ns_string(&token)];
        }
        let _: () = msg_send![alert, setAccessoryView: text_field];
        let alert_window: id = msg_send![alert, window];
        let _: () = msg_send![alert_window, makeKeyAndOrderFront: nil];
        let _: () = msg_send![alert_window, setInitialFirstResponder: text_field];
        let _: () = msg_send![alert_window, makeFirstResponder: text_field];

        let response: i64 = msg_send![alert, runModal];
        match response {
            NS_ALERT_FIRST_BUTTON_RETURN => {
                let token: id = msg_send![text_field, stringValue];
                let token = ns_string_to_rust(token).trim().to_owned();
                if token.is_empty() {
                    clear_token()?;
                    return Ok(());
                }

                VercelClient::new(&token)
                    .and_then(|client| client.validate_token())
                    .context("The provided Vercel token is invalid")?;
                save_token(&token)?;
                clear_snapshot_cache();
                Ok(())
            }
            NS_ALERT_THIRD_BUTTON_RETURN => {
                clear_token()?;
                clear_snapshot_cache();
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

unsafe fn show_error_alert(title: &str, message: &str) {
    let app = NSApp();
    let _: () = msg_send![app, activateIgnoringOtherApps: YES];

    let alert: id = msg_send![class!(NSAlert), new];
    let _: () = msg_send![alert, setMessageText: ns_string(title)];
    let _: () = msg_send![alert, setInformativeText: ns_string(message)];
    let _: id = msg_send![alert, addButtonWithTitle: ns_string("OK")];
    let _: i64 = msg_send![alert, runModal];
}

fn load_token() -> Result<Option<String>> {
    if let Ok(token) = std::env::var("VERCEL_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_owned()));
        }
    }
    let entry = token_entry()?;
    match entry.get_password() {
        Ok(token) if token.trim().is_empty() => Ok(None),
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err).context("Keychain lookup failed"),
    }
}

fn save_token(token: &str) -> Result<()> {
    token_entry()?
        .set_password(token)
        .context("Failed to save token to the macOS keychain")
}

fn clear_token() -> Result<()> {
    let entry = token_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err).context("Failed to clear saved token"),
    }
}

fn token_entry() -> Result<Entry> {
    Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("Failed to open keychain entry")
}

fn open_url(url: &str) -> Result<()> {
    let status = Command::new("open")
        .arg(url)
        .status()
        .with_context(|| format!("Failed to launch open for {url}"))?;
    if !status.success() {
        anyhow::bail!("open exited with status {status}");
    }
    Ok(())
}

unsafe fn ns_string(value: &str) -> id {
    NSString::alloc(nil).init_str(value)
}

unsafe fn ns_string_to_rust(value: id) -> String {
    let utf8: *const c_char = msg_send![value, UTF8String];
    if utf8.is_null() {
        String::new()
    } else {
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

unsafe fn new_controller() -> id {
    let class = menu_controller_class();
    let controller: id = msg_send![class, new];
    controller
}

fn menu_controller_class() -> &'static Class {
    static CLASS: OnceCell<&'static Class> = OnceCell::new();
    CLASS.get_or_init(|| unsafe {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("DevTreeMenuController", superclass)
            .expect("Failed to declare DevTreeMenuController");
        decl.add_method(
            sel!(menuWillOpen:),
            menu_will_open as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(refreshMenu:),
            refresh_menu as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(autoRefresh:),
            auto_refresh as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(loadMenu:),
            load_menu as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(openSettings:),
            open_settings as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(openRepresentedUrl:),
            open_represented_url as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(sel!(quitApp:), quit_app as extern "C" fn(&Object, Sel, id));
        decl.add_method(sel!(noop:), noop as extern "C" fn(&Object, Sel, id));
        decl.register()
    })
}

extern "C" fn noop(_: &Object, _: Sel, _: id) {}
