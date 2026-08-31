#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::{Context, Result};
use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength,
};
use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSRect, NSSize, NSString};
use keyring::Entry;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use once_cell::sync::OnceCell;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::{
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::vercel::{AppSnapshot, ProjectSnapshot, TeamSnapshot, VercelClient};

const KEYRING_SERVICE: &str = "dev-tree";
const KEYRING_ACCOUNT: &str = "vercel-token";
const TOKEN_DEFAULTS_KEY: &str = "vercelToken";
const SETTINGS_SAVE: i64 = 1;
const SETTINGS_CLEAR: i64 = 2;
const PROJECT_SORT_KEY: &str = "projectSortOrder";
const SORT_LAST_DEPLOYMENT: i64 = 0;
const SORT_ALPHABETICAL: i64 = 1;
const CACHE_TTL: Duration = Duration::from_secs(300);
static SNAPSHOT_CACHE: OnceCell<Mutex<Option<(Instant, AppSnapshot)>>> = OnceCell::new();
static STATUS_ITEM: AtomicUsize = AtomicUsize::new(0);
static APP_MENU: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_WINDOW: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_FIELD: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_SORT: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_RESPONSE: AtomicUsize = AtomicUsize::new(0);

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
        let _: id = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.01f64
            target: controller
            selector: sel!(loadMenu:)
            userInfo: menu
            repeats: NO
        ];
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
        match show_settings_modal(this as *const _ as id) {
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
    if cached_snapshot().is_some() {
        return;
    }
    unsafe {
        let menu = APP_MENU.load(Ordering::Relaxed) as id;
        if menu != nil {
            rebuild_menu(menu, this as *const _ as id);
        }
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
    add_static_item(menu, "Add a Vercel token in Settings.");
    add_footer_items(menu, controller);
}

unsafe fn populate_error_menu(menu: id, controller: id, message: &str) {
    clear_menu(menu);
    add_static_item(menu, message);
    add_footer_items(menu, controller);
}

unsafe fn add_team_menu(menu: id, controller: id, team: &TeamSnapshot) {
    add_separator(menu);
    let title = format!("{} ({})", team.name, team.projects.len());
    let item = make_menu_item(&title);
    let image: id = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string("folder")
        accessibilityDescription: ns_string("Team")
    ];
    if image != nil {
        let _: () = msg_send![item, setImage: image];
    }
    let _: () = msg_send![item, setEnabled: NO];
    let _: () = msg_send![menu, addItem: item];
    let mut projects = team.projects.clone();
    if project_sort_order() == SORT_ALPHABETICAL {
        projects.sort_by_key(|project| project.name.to_ascii_lowercase());
    } else {
        projects.sort_by_key(|project| std::cmp::Reverse(project.latest_deployment_at()));
    }
    for project in &projects {
        add_project_menu(menu, controller, project);
    }
}

unsafe fn add_project_menu(menu: id, controller: id, project: &ProjectSnapshot) {
    let title = project.name.clone();
    let project_item = make_menu_item(&title);
    let status_color: id = match project.latest_status {
        crate::vercel::DeploymentStatus::Healthy => msg_send![class!(NSColor), systemGreenColor],
        crate::vercel::DeploymentStatus::Failed => msg_send![class!(NSColor), systemRedColor],
        crate::vercel::DeploymentStatus::Pending => msg_send![class!(NSColor), systemOrangeColor],
        crate::vercel::DeploymentStatus::Missing => msg_send![class!(NSColor), secondaryLabelColor],
    };
    let status_image = colored_symbol_image(
        match project.latest_status {
            crate::vercel::DeploymentStatus::Healthy => "checkmark.circle.fill",
            crate::vercel::DeploymentStatus::Failed => "xmark.circle.fill",
            crate::vercel::DeploymentStatus::Pending => "circle.dotted",
            crate::vercel::DeploymentStatus::Missing => "circle",
        },
        status_color,
        "Deployment status",
    );
    if status_image != nil {
        let _: () = msg_send![project_item, setImage: status_image];
    }

    unsafe fn colored_symbol_image(symbol: &str, color: id, description: &str) -> id {
        let image: id = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string(symbol)
            accessibilityDescription: ns_string(description)
        ];
        if image == nil {
            return nil;
        }
        let configuration: id = msg_send![
            class!(NSImageSymbolConfiguration),
            configurationWithHierarchicalColor: color
        ];
        let configured: id = msg_send![image, imageWithSymbolConfiguration: configuration];
        if configured == nil { image } else { configured }
    }
    let project_menu = NSMenu::new(nil);
    let _: () = msg_send![project_menu, setAutoenablesItems: NO];

    add_deployment_item(project_menu, &project.latest_status_label);
    add_separator(project_menu);

    add_url_item(
        project_menu,
        "Open project dashboard",
        &project.dashboard_url,
        controller,
    );

    if let Some(url) = &project.latest_deployment_url {
        add_url_item(project_menu, "Open latest deployment", url, controller);
    }

    add_url_item(
        project_menu,
        "Open website logs",
        &project.logs_url,
        controller,
    );
    if let Some(url) = &project.production_url {
        add_separator(project_menu);
        add_url_item(project_menu, "Visit production site", url, controller);
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
    guard
        .as_ref()
        .filter(|(time, _)| time.elapsed() < CACHE_TTL)
        .map(|(_, snapshot)| snapshot.clone())
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

unsafe fn add_deployment_item(menu: id, label: &str) {
    let title = label.strip_prefix("Deployment: ").unwrap_or(label);
    let item = make_menu_item(title);
    let image: id = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string("calendar")
        accessibilityDescription: ns_string("Deployment date")
    ];
    if image != nil {
        let _: () = msg_send![item, setImage: image];
    }
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
    let symbol = match title {
        "Refresh" => Some("arrow.clockwise"),
        "Settings…" => Some("gearshape"),
        "Quit" => Some("power"),
        _ => None,
    };
    if let Some(symbol) = symbol {
        let image: id = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string(symbol)
            accessibilityDescription: ns_string(title)
        ];
        if image != nil {
            let _: () = msg_send![item, setImage: image];
        }
    }
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

unsafe fn add_settings_symbol(content: id, symbol: &str, description: &str, y: f64) {
    let image: id = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string(symbol)
        accessibilityDescription: ns_string(description)
    ];
    if image == nil {
        return;
    }
    let image_view: id = msg_send![class!(NSImageView), alloc];
    let image_view: id = msg_send![
        image_view,
        initWithFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(24.0, y + 1.0),
            NSSize::new(18.0, 18.0)
        )
    ];
    let _: () = msg_send![image_view, setImage: image];
    let _: () = msg_send![image_view, setImageScaling: 3i64];
    let _: () = msg_send![image_view, setImageAlignment: 0i64];
    let tint: id = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![image_view, setContentTintColor: tint];
    let _: () = msg_send![content, addSubview: image_view];
}

unsafe fn settings_heading_font() -> id {
    let rounded: id = msg_send![
        class!(NSFont),
        fontWithName: ns_string("SF Pro Rounded Semibold")
        size: 16.0f64
    ];
    if rounded != nil {
        rounded
    } else {
        msg_send![class!(NSFont), systemFontOfSize: 16.0f64 weight: 0.4f64]
    }
}

fn show_settings_modal(controller: id) -> Result<()> {
    unsafe {
        let app = NSApp();
        let _: () = msg_send![app, activateIgnoringOtherApps: YES];
        let window: id = msg_send![class!(NSWindow), alloc];
        let window: id = msg_send![window, initWithContentRect: NSRect::new(
            cocoa::foundation::NSPoint::new(0.0, 0.0), NSSize::new(500.0, 350.0)
        ) styleMask: (1u64 | 2u64) backing: 2u64 defer: NO];
        let _: () = msg_send![window, setTitle: ns_string("Settings")];
        let _: () = msg_send![window, setReleasedWhenClosed: NO];
        let close_button: id = msg_send![window, standardWindowButton: 2i64];
        let _: () = msg_send![close_button, setTarget: controller];
        let _: () = msg_send![close_button, setAction: sel!(settingsCancel:)];
        let _: () = msg_send![window, center];
        let content: id = msg_send![window, contentView];

        add_settings_symbol(content, "person.crop.circle", "Vercel account", 294.0);
        let account_label: id =
            msg_send![class!(NSTextField), labelWithString: ns_string("Vercel Account")];
        let _: () = msg_send![account_label, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(56.0, 292.0), NSSize::new(300.0, 22.0)
        )];
        let heading_font = settings_heading_font();
        let _: () = msg_send![account_label, setFont: heading_font];
        let _: () = msg_send![content, addSubview: account_label];
        let label: id = msg_send![class!(NSTextField), labelWithString: ns_string(
            "Vercel token (used to load teams and projects):"
        )];
        let _: () = msg_send![label, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(56.0, 266.0), NSSize::new(420.0, 20.0)
        )];
        let secondary_color: id = msg_send![class!(NSColor), secondaryLabelColor];
        let _: () = msg_send![label, setTextColor: secondary_color];
        let _: () = msg_send![content, addSubview: label];

        let text_field: id = msg_send![class!(NSSecureTextField), alloc];
        let text_field: id = msg_send![text_field, initWithFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(56.0, 222.0), NSSize::new(420.0, 32.0)
        )];
        let _: () = msg_send![text_field, setEditable: YES];
        let _: () = msg_send![text_field, setSelectable: YES];
        let _: () = msg_send![text_field, setPlaceholderString: ns_string("Paste a Vercel token")];
        if let Some(token) = load_token()? {
            let _: () = msg_send![text_field, setStringValue: ns_string(&token)];
        }
        let _: () = msg_send![content, addSubview: text_field];

        let clear: id = msg_send![class!(NSButton), buttonWithTitle: ns_string("Clear Token") target: controller action: sel!(settingsClear:)];
        let _: () = msg_send![clear, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(56.0, 180.0), NSSize::new(112.0, 28.0)
        )];
        let _: () = msg_send![content, addSubview: clear];

        add_settings_symbol(content, "square.grid.2x2", "Project display", 132.0);
        let display_label: id =
            msg_send![class!(NSTextField), labelWithString: ns_string("Project Display")];
        let _: () = msg_send![display_label, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(56.0, 130.0), NSSize::new(420.0, 22.0)
        )];
        let _: () = msg_send![display_label, setFont: heading_font];
        let _: () = msg_send![content, addSubview: display_label];
        let sort_label: id =
            msg_send![class!(NSTextField), labelWithString: ns_string("Projects sort order")];
        let _: () = msg_send![sort_label, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(56.0, 99.0), NSSize::new(160.0, 22.0)
        )];
        let _: () = msg_send![content, addSubview: sort_label];
        let sort_popup: id = msg_send![class!(NSPopUpButton), alloc];
        let sort_popup: id = msg_send![sort_popup, initWithFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(270.0, 100.0), NSSize::new(206.0, 28.0)
        ) pullsDown: NO];
        let _: () = msg_send![sort_popup, addItemWithTitle: ns_string("Last deployment date")];
        let _: () = msg_send![sort_popup, addItemWithTitle: ns_string("Alphabetically")];
        let _: () = msg_send![sort_popup, selectItemAtIndex: project_sort_order()];
        let _: () = msg_send![content, addSubview: sort_popup];

        let cancel: id = msg_send![class!(NSButton), buttonWithTitle: ns_string("Cancel") target: controller action: sel!(settingsCancel:)];
        let _: () = msg_send![cancel, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(330.0, 20.0), NSSize::new(70.0, 32.0)
        )];
        let save: id = msg_send![class!(NSButton), buttonWithTitle: ns_string("Save") target: controller action: sel!(settingsSave:)];
        let _: () = msg_send![save, setFrame: NSRect::new(
            cocoa::foundation::NSPoint::new(414.0, 20.0), NSSize::new(70.0, 32.0)
        )];
        let _: () = msg_send![save, setKeyEquivalent: ns_string("\r")];
        let _: () = msg_send![save, setKeyEquivalentModifierMask: 0u64];
        let _: () = msg_send![cancel, setKeyEquivalent: ns_string("\u{1b}")];
        let _: () = msg_send![cancel, setKeyEquivalentModifierMask: 0u64];
        let _: () = msg_send![save, setBezelStyle: 1i64];
        let _: () = msg_send![content, addSubview: cancel];
        let _: () = msg_send![content, addSubview: save];
        SETTINGS_WINDOW.store(window as usize, Ordering::Relaxed);
        SETTINGS_FIELD.store(text_field as usize, Ordering::Relaxed);
        SETTINGS_SORT.store(sort_popup as usize, Ordering::Relaxed);
        SETTINGS_RESPONSE.store(0, Ordering::Relaxed);
        let _: () = msg_send![window, makeKeyAndOrderFront: nil];
        let _: () = msg_send![window, makeFirstResponder: text_field];
        let _: () = msg_send![app, runModalForWindow: window];
        let response = SETTINGS_RESPONSE.load(Ordering::Relaxed) as i64;
        let _: () = msg_send![window, orderOut: nil];
        SETTINGS_WINDOW.store(0, Ordering::Relaxed);
        SETTINGS_FIELD.store(0, Ordering::Relaxed);
        SETTINGS_SORT.store(0, Ordering::Relaxed);
        if response == SETTINGS_SAVE {
            Ok(())
        } else if response == SETTINGS_CLEAR {
            clear_token()?;
            clear_snapshot_cache();
            Ok(())
        } else {
            Ok(())
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
    if let Ok(token) = std::env::var("VERCEL_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(Some(token.trim().to_owned()));
    }
    let entry = token_entry()?;
    match entry.get_password() {
        Ok(token) if token.trim().is_empty() => Ok(None),
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => load_token_from_defaults(),
        Err(err) => Err(err).context("Keychain lookup failed"),
    }
}

fn save_token(token: &str) -> Result<()> {
    token_entry()?
        .set_password(token)
        .context("Failed to save token to the macOS keychain")?;
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let _: () = msg_send![
            defaults,
            setObject: ns_string(token)
            forKey: ns_string(TOKEN_DEFAULTS_KEY)
        ];
    }
    Ok(())
}

fn clear_token() -> Result<()> {
    let entry = token_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            unsafe {
                let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
                let _: () = msg_send![defaults, removeObjectForKey: ns_string(TOKEN_DEFAULTS_KEY)];
            }
            Ok(())
        }
        Err(err) => Err(err).context("Failed to clear saved token"),
    }
}

fn load_token_from_defaults() -> Result<Option<String>> {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: id = msg_send![defaults, objectForKey: ns_string(TOKEN_DEFAULTS_KEY)];
        if value == nil {
            Ok(None)
        } else {
            let token = ns_string_to_rust(value);
            if token.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(token))
            }
        }
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
        decl.add_method(
            sel!(settingsSave:),
            settings_save as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(settingsCancel:),
            settings_cancel as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(settingsClear:),
            settings_clear as extern "C" fn(&Object, Sel, id),
        );
        decl.register()
    })
}

extern "C" fn noop(_: &Object, _: Sel, _: id) {}

extern "C" fn settings_save(_: &Object, _: Sel, _: id) {
    unsafe {
        let field = SETTINGS_FIELD.load(Ordering::Relaxed) as id;
        let token = normalize_token(&ns_string_to_rust(msg_send![field, stringValue]));
        let result = if token.is_empty() {
            clear_token()
        } else {
            VercelClient::new(&token)
                .and_then(|client| client.validate_token())
                .context("The provided Vercel token is invalid")
                .and_then(|_| save_token(&token))
        };
        match result {
            Ok(()) => {
                let sort_popup = SETTINGS_SORT.load(Ordering::Relaxed) as id;
                let sort: i64 = msg_send![sort_popup, indexOfSelectedItem];
                set_project_sort_order(sort);
                clear_snapshot_cache();
                SETTINGS_RESPONSE.store(SETTINGS_SAVE as usize, Ordering::Relaxed);
                let _: () = msg_send![NSApp(), stopModal];
            }

            Err(err) => show_error_alert("Failed to update Vercel token", &err.to_string()),
        }
    }
}

fn normalize_token(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix('#').unwrap_or(value).trim();
    let value = value
        .strip_prefix("VERCEL_TOKEN=")
        .unwrap_or(value)
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    value.split_whitespace().collect()
}

extern "C" fn settings_cancel(_: &Object, _: Sel, _: id) {
    SETTINGS_RESPONSE.store(0, Ordering::Relaxed);
    unsafe {
        let _: () = msg_send![NSApp(), stopModal];
    }
}

extern "C" fn settings_clear(_: &Object, _: Sel, _: id) {
    SETTINGS_RESPONSE.store(SETTINGS_CLEAR as usize, Ordering::Relaxed);
    unsafe {
        let _: () = msg_send![NSApp(), stopModal];
    }
}

fn project_sort_order() -> i64 {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: i64 = msg_send![defaults, integerForKey: ns_string(PROJECT_SORT_KEY)];
        if value == SORT_ALPHABETICAL {
            SORT_ALPHABETICAL
        } else {
            SORT_LAST_DEPLOYMENT
        }
    }
}

fn set_project_sort_order(order: i64) {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let _: () = msg_send![defaults, setInteger: order forKey: ns_string(PROJECT_SORT_KEY)];
        let _: () = msg_send![defaults, synchronize];
    }
}
