#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::{Context, Result};
use keyring::Entry;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSButton, NSColor, NSMenu, NSStatusBar,
    NSVariableStatusItemLength,
};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString};
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

type ObjcId = *mut AnyObject;
const NIL: ObjcId = std::ptr::null_mut();

const KEYRING_SERVICE: &str = "dev-tree";
const KEYRING_ACCOUNT: &str = "vercel-token";
const TOKEN_DEFAULTS_KEY: &str = "vercelToken";
const VERCEL_TOKEN_SETTINGS_URL: &str = "https://vercel.com/account/settings/tokens";
const SETTINGS_SAVE: i64 = 1;
const SETTINGS_CLEAR: i64 = 2;
const PROJECT_SORT_KEY: &str = "projectSortOrder";
const VISIBILITY_KEY_PREFIX: &str = "visibility";
const SORT_LAST_DEPLOYMENT: i64 = 0;
const SORT_ALPHABETICAL: i64 = 1;
const CACHE_TTL: Duration = Duration::from_secs(300);
const COMMAND_MODIFIER_MASK: u64 = 1 << 20;

static SNAPSHOT_CACHE: OnceCell<Mutex<Option<(Instant, AppSnapshot)>>> = OnceCell::new();
static STATUS_ITEM: AtomicUsize = AtomicUsize::new(0);
static APP_MENU: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_WINDOW: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_FIELD: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_SORT: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_RESPONSE: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_VISIBILITY: OnceCell<Mutex<Vec<(String, usize)>>> = OnceCell::new();
static SETTINGS_CONTENT: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_TREE: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_SNAPSHOT: OnceCell<Mutex<Option<AppSnapshot>>> = OnceCell::new();
static SETTINGS_PENDING_VISIBILITY: OnceCell<Mutex<Vec<(String, bool)>>> = OnceCell::new();
static SETTINGS_ORIGINAL_TOKEN: OnceCell<Mutex<Option<String>>> = OnceCell::new();

macro_rules! send_void {
    ($receiver:expr, $($message:tt)*) => {{
        let _: () = msg_send![$receiver, $($message)*];
    }};
}

struct AppLifetime {
    _controller: ObjcId,
    _status_item: ObjcId,
}

fn app() -> ObjcId {
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    Retained::as_ptr(&app).cast_mut().cast()
}

fn new_menu() -> ObjcId {
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    Retained::into_raw(NSMenu::new(mtm)).cast()
}

pub fn run() {
    unsafe {
        dotenvy::dotenv().ok();

        let _pool = NSAutoreleasePool::new();
        let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let controller = new_controller();
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        let status_item: ObjcId = Retained::into_raw(status_item).cast();

        STATUS_ITEM.store(status_item as usize, Ordering::Relaxed);

        let button: ObjcId = msg_send![status_item, button];
        set_status_icon(button, "triangle.fill");

        let menu = new_menu();
        send_void![menu, setAutoenablesItems: Bool::NO];
        send_void![menu, setDelegate: controller];
        populate_loading_menu(menu, controller);
        send_void![status_item, setMenu: menu];

        APP_MENU.store(menu as usize, Ordering::Relaxed);

        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.01f64,
            target: controller,
            selector: sel!(loadMenu:),
            userInfo: menu,
            repeats: Bool::NO,
        ];
        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 300.0f64,
            target: controller,
            selector: sel!(autoRefresh:),
            userInfo: NIL,
            repeats: Bool::YES,
        ];

        let lifetime = Box::new(AppLifetime {
            _controller: controller,
            _status_item: status_item,
        });
        Box::leak(lifetime);

        app.run();
    }
}

unsafe fn populate_loading_menu(menu: ObjcId, controller: ObjcId) {
    clear_menu(menu);
    add_static_item(menu, "Loading…");
    add_separator(menu);
    add_action_item(menu, "Settings…", sel!(openSettings:), controller);
    add_action_item(menu, "Quit", sel!(quitApp:), controller);
}

extern "C" fn menu_will_open(this: ObjcId, _: Sel, menu: ObjcId) {
    unsafe {
        clear_project_shortcuts(APP_MENU.load(Ordering::Relaxed) as ObjcId);
        configure_project_shortcuts(menu);
    }
    if cached_snapshot().is_some() {
        return;
    }
    unsafe {
        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.01f64,
            target: this,
            selector: sel!(loadMenu:),
            userInfo: menu,
            repeats: Bool::NO,
        ];
    }
}

extern "C" fn load_menu(this: ObjcId, _: Sel, timer: ObjcId) {
    unsafe {
        let menu: ObjcId = msg_send![timer, userInfo];
        rebuild_menu(menu, this);
    }
}

extern "C" fn refresh_menu(this: ObjcId, _: Sel, sender: ObjcId) {
    unsafe {
        let menu: ObjcId = msg_send![sender, menu];
        if let Some(cache) = SNAPSHOT_CACHE.get() {
            *cache.lock().unwrap() = None;
        }
        set_status_icon_for_item("arrow.3.trianglepath");
        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.01f64,
            target: this,
            selector: sel!(loadMenu:),
            userInfo: menu,
            repeats: Bool::NO,
        ];
    }
}

extern "C" fn open_settings(this: ObjcId, _: Sel, _: ObjcId) {
    unsafe {
        match show_settings_modal(this) {
            Ok(()) => {
                let menu = APP_MENU.load(Ordering::Relaxed) as ObjcId;
                if menu != NIL {
                    rebuild_menu(menu, this);
                }
            }
            Err(err) => show_error_alert("Failed to update Vercel token", &err.to_string()),
        }
    }
}

extern "C" fn open_represented_url(_: ObjcId, _: Sel, sender: ObjcId) {
    unsafe {
        let represented: ObjcId = msg_send![sender, representedObject];
        if represented == NIL {
            return;
        }

        if let Err(err) = open_url(&ns_string_to_rust(represented)) {
            show_error_alert("Failed to open link", &err.to_string());
        }
    }
}

extern "C" fn open_token_settings(_: ObjcId, _: Sel, _: ObjcId) {
    if let Err(err) = open_url(VERCEL_TOKEN_SETTINGS_URL) {
        unsafe {
            show_error_alert("Failed to open Vercel token settings", &err.to_string());
        }
    }
}

extern "C" fn quit_app(_: ObjcId, _: Sel, _: ObjcId) {
    unsafe {
        let app = app();
        send_void![app, terminate: NIL];
    }
}

unsafe fn rebuild_menu(menu: ObjcId, controller: ObjcId) {
    send_void![app(), updateWindows];
    set_status_icon_for_item("arrow.3.trianglepath");
    clear_menu(menu);
    add_static_item(menu, "Refreshing Vercel projects…");

    match load_token() {
        Ok(Some(token)) => {
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
    send_void![menu, update];
}

extern "C" fn auto_refresh(this: ObjcId, _: Sel, _: ObjcId) {
    if cached_snapshot().is_some() {
        return;
    }
    unsafe {
        let menu = APP_MENU.load(Ordering::Relaxed) as ObjcId;
        if menu != NIL {
            rebuild_menu(menu, this);
        }
    }
}

unsafe fn set_status_icon_for_item(symbol: &str) {
    let status_item = STATUS_ITEM.load(Ordering::Relaxed) as ObjcId;
    if status_item != NIL {
        let button: ObjcId = msg_send![status_item, button];
        set_status_icon(button, symbol);
    }
}

unsafe fn set_status_icon(button: ObjcId, symbol: &str) {
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string(symbol),
        accessibilityDescription: ns_string("Vercel Bar"),
    ];
    if image == NIL {
        return;
    }
    send_void![button, setImage: image];
    send_void![button, setTitle: ns_string("")];
    send_void![button, displayIfNeeded];
    send_void![app(), updateWindows];
}

unsafe fn populate_snapshot_menu(menu: ObjcId, controller: ObjcId, snapshot: AppSnapshot) {
    set_status_icon_for_item("triangle.fill");
    clear_menu(menu);

    if snapshot.teams.is_empty() {
        add_static_item(menu, "No projects found for this Vercel account.");
    } else {
        for team in snapshot.teams {
            if is_visible(&visibility_key("team", &team.name)) {
                add_team_menu(menu, controller, &team);
            }
        }
    }

    add_footer_items(menu, controller);
}

unsafe fn populate_missing_token_menu(menu: ObjcId, controller: ObjcId) {
    set_status_icon_for_item("info.triangle.fill");
    clear_menu(menu);
    add_static_item(menu, "Add a Vercel token in Settings.");
    add_footer_items(menu, controller);
}

unsafe fn populate_error_menu(menu: ObjcId, controller: ObjcId, message: &str) {
    clear_menu(menu);
    add_static_item(menu, message);
    add_footer_items(menu, controller);
}

unsafe fn add_team_menu(menu: ObjcId, controller: ObjcId, team: &TeamSnapshot) {
    add_separator(menu);
    let visible_projects = team
        .projects
        .iter()
        .filter(|project| {
            is_visible(&visibility_key(
                "project",
                &format!("{}::{}", team.name, project.name),
            ))
        })
        .count();
    let title = if visible_projects == team.projects.len() {
        team.name.clone()
    } else {
        format!("{} ({visible_projects}/{})", team.name, team.projects.len())
    };
    let item = make_menu_item(&title);
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string("folder"),
        accessibilityDescription: ns_string("Team"),
    ];
    if image != NIL {
        send_void![item, setImage: image];
    }
    send_void![item, setEnabled: Bool::NO];
    send_void![menu, addItem: item];
    let mut projects = team.projects.clone();
    if project_sort_order() == SORT_ALPHABETICAL {
        projects.sort_by_key(|project| project.name.to_ascii_lowercase());
    } else {
        projects.sort_by_key(|project| std::cmp::Reverse(project.latest_deployment_at()));
    }
    for project in &projects {
        if is_visible(&visibility_key(
            "project",
            &format!("{}::{}", team.name, project.name),
        )) {
            add_project_menu(menu, controller, project);
        }
    }
}

unsafe fn add_project_menu(menu: ObjcId, controller: ObjcId, project: &ProjectSnapshot) {
    let title = project.name.clone();
    let project_item = make_menu_item(&title);
    let status_color: ObjcId = match project.latest_status {
        crate::vercel::DeploymentStatus::Healthy => msg_send![class!(NSColor), systemGreenColor],
        crate::vercel::DeploymentStatus::Failed => msg_send![class!(NSColor), systemRedColor],
        crate::vercel::DeploymentStatus::Pending => msg_send![class!(NSColor), systemOrangeColor],
        crate::vercel::DeploymentStatus::Missing => msg_send![class!(NSColor), secondaryLabelColor],
    };
    let status_image = colored_symbol_image(
        match project.latest_status {
            crate::vercel::DeploymentStatus::Healthy => "checkmark.circle.fill",
            crate::vercel::DeploymentStatus::Failed => "xmark.circle.fill",
            crate::vercel::DeploymentStatus::Pending => "circle.circle.fill",
            crate::vercel::DeploymentStatus::Missing => "circle",
        },
        status_color,
        "Deployment status",
    );
    if status_image != NIL {
        send_void![project_item, setImage: status_image];
    }

    unsafe fn colored_symbol_image(symbol: &str, color: ObjcId, description: &str) -> ObjcId {
        let image: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string(symbol),
            accessibilityDescription: ns_string(description),
        ];
        if image == NIL {
            return NIL;
        }
        let configuration: ObjcId = msg_send![
            class!(NSImageSymbolConfiguration),
            configurationWithHierarchicalColor: color
        ];
        let configured: ObjcId = msg_send![image, imageWithSymbolConfiguration: configuration];
        if configured == NIL { image } else { configured }
    }
    let project_menu = new_menu();
    send_void![project_menu, setAutoenablesItems: Bool::NO];
    send_void![project_menu, setDelegate: controller];

    add_url_item(
        project_menu,
        "Open project dashboard",
        &project.dashboard_url,
        controller,
    );

    add_separator(project_menu);
    if let Some(url) = &project.production_url {
        add_url_item(project_menu, "Open production website", url, controller);
        add_url_item(project_menu, "Open production logs", &project.logs_url, controller);
    }

    add_separator(project_menu);
    if let Some(url) = &project.latest_deployment_url {
        add_url_item(project_menu, "Open latest deployment", url, controller);
        add_separator(project_menu);
    }
    add_menu_header(project_menu, "Deployment information");
    add_deployment_item(project_menu, &project.latest_status_label);
    if let Some(page_views) = project.page_views {
        add_page_views_item(project_menu, page_views);
    }

    send_void![project_item, setSubmenu: project_menu];
    send_void![menu, addItem: project_item];
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

unsafe fn add_footer_items(menu: ObjcId, controller: ObjcId) {
    add_separator(menu);
    add_action_item(menu, "Refresh", sel!(refreshMenu:), controller);
    add_action_item(menu, "Settings…", sel!(openSettings:), controller);
    add_action_item(menu, "Quit", sel!(quitApp:), controller);
}

unsafe fn clear_menu(menu: ObjcId) {
    loop {
        let count: isize = msg_send![menu, numberOfItems];
        if count <= 0 {
            break;
        }

        send_void![menu, removeItemAtIndex: 0isize];
    }
}

unsafe fn add_separator(menu: ObjcId) {
    let separator: ObjcId = msg_send![class!(NSMenuItem), separatorItem];
    send_void![menu, addItem: separator];
}

unsafe fn add_menu_header(menu: ObjcId, title: &str) {
    let header: ObjcId = msg_send![
        class!(NSMenuItem),
        sectionHeaderWithTitle: ns_string(title)
    ];
    send_void![menu, addItem: header];
}

unsafe fn add_static_item(menu: ObjcId, title: &str) {
    let item = make_menu_item(title);
    if title == "Add a Vercel token in Settings." {
        let image: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("info.circle"),
            accessibilityDescription: ns_string("Information"),
        ];
        if image != NIL {
            send_void![item, setImage: image];
        }
    }
    send_void![item, setEnabled: Bool::NO];
    send_void![menu, addItem: item];
}

unsafe fn add_deployment_item(menu: ObjcId, label: &str) {
    let title = label.strip_prefix("Deployment: ").unwrap_or(label);
    let item = make_menu_item(title);
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string("calendar"),
        accessibilityDescription: ns_string("Deployment date"),
    ];
    if image != NIL {
        send_void![item, setImage: image];
    }
    send_void![item, setEnabled: Bool::NO];
    send_void![menu, addItem: item];
}

unsafe fn add_page_views_item(menu: ObjcId, page_views: u64) {
    let title = format!("{} (last 7d)", format_number(page_views));
    let item = make_menu_item(&title);
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string("person.circle"),
        accessibilityDescription: ns_string("Page views"),
    ];
    if image != NIL {
        send_void![item, setImage: image];
    }
    send_void![item, setEnabled: Bool::NO];
    send_void![menu, addItem: item];
}

fn format_number(value: u64) -> String {
    let digits = value.to_string();
    let first_group_len = digits.len() % 3;
    let first_group_len = if first_group_len == 0 {
        3
    } else {
        first_group_len
    };
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    formatted.push_str(&digits[..first_group_len]);
    for chunk in digits.as_bytes()[first_group_len..].chunks(3) {
        formatted.push(',');
        formatted.push_str(std::str::from_utf8(chunk).expect("numeric digits are valid UTF-8"));
    }
    formatted
}

unsafe fn add_action_item(menu: ObjcId, title: &str, action: Sel, controller: ObjcId) {
    let key_equivalent = match title {
        "Refresh" => "r",
        "Settings…" => ",",
        "Quit" => "q",
        _ => "",
    };
    let item: ObjcId = msg_send![class!(NSMenuItem), alloc];
    let item: ObjcId = msg_send![
        item,
        initWithTitle: ns_string(title),
        action: action,
        keyEquivalent: ns_string(key_equivalent),
    ];
    send_void![item, setTarget: controller];
    if !key_equivalent.is_empty() {
        send_void![item, setKeyEquivalentModifierMask: COMMAND_MODIFIER_MASK];
    }
    let symbol = match title {
        "Refresh" => Some("arrow.clockwise"),
        "Settings…" => Some("gearshape"),
        "Quit" => Some("power"),
        _ => None,
    };
    if let Some(symbol) = symbol {
        let image: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string(symbol),
            accessibilityDescription: ns_string(title),
        ];
        if image != NIL {
            send_void![item, setImage: image];
        }
    }
    send_void![menu, addItem: item];
}

unsafe fn add_url_item(menu: ObjcId, title: &str, url: &str, controller: ObjcId) {
    let item: ObjcId = msg_send![class!(NSMenuItem), alloc];
    let item: ObjcId = msg_send![
        item,
        initWithTitle: ns_string(title),
        action: sel!(openRepresentedUrl:),
        keyEquivalent: ns_string(""),
    ];
    send_void![item, setTarget: controller];
    send_void![item, setRepresentedObject: ns_string(url)];
    send_void![menu, addItem: item];
}

unsafe fn configure_project_shortcuts(menu: ObjcId) {
    let count: isize = msg_send![menu, numberOfItems];
    for index in 0..count {
        let item: ObjcId = msg_send![menu, itemAtIndex: index];
        if item == NIL {
            continue;
        }
        let title: ObjcId = msg_send![item, title];
        let key_equivalent = match ns_string_to_rust(title).as_str() {
            "Open project dashboard" => Some("p"),
            "Open production logs" => Some("l"),
            "Open latest deployment" => Some("d"),
            _ => None,
        };
        if let Some(key_equivalent) = key_equivalent {
            send_void![item, setKeyEquivalent: ns_string(key_equivalent)];
            send_void![item, setKeyEquivalentModifierMask: COMMAND_MODIFIER_MASK];
        }
    }
}

unsafe fn clear_project_shortcuts(menu: ObjcId) {
    if menu == NIL {
        return;
    }

    let count: isize = msg_send![menu, numberOfItems];
    for index in 0..count {
        let item: ObjcId = msg_send![menu, itemAtIndex: index];
        if item == NIL {
            continue;
        }

        let title: ObjcId = msg_send![item, title];
        match ns_string_to_rust(title).as_str() {
            "Open project dashboard" | "Open production logs" | "Open latest deployment" => {
                send_void![item, setKeyEquivalent: ns_string("")];
                send_void![item, setKeyEquivalentModifierMask: 0u64];
            }
            _ => {}
        }

        let submenu: ObjcId = msg_send![item, submenu];
        clear_project_shortcuts(submenu);
    }
}

unsafe fn make_menu_item(title: &str) -> ObjcId {
    let item: ObjcId = msg_send![class!(NSMenuItem), alloc];
    msg_send![
        item,
        initWithTitle: ns_string(title),
        action: sel!(noop:),
        keyEquivalent: ns_string(""),
    ]
}

unsafe fn add_settings_symbol(content: ObjcId, symbol: &str, description: &str, y: f64) {
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string(symbol),
        accessibilityDescription: ns_string(description),
    ];
    if image == NIL {
        return;
    }
    let image_view: ObjcId = msg_send![class!(NSImageView), alloc];
    let image_view: ObjcId = msg_send![
        image_view,
        initWithFrame: NSRect::new(
            NSPoint::new(24.0, y + 2.0),
            NSSize::new(20.0, 20.0)
        )
    ];
    send_void![image_view, setImage: image];
    send_void![image_view, setImageScaling: 3i64];
    send_void![image_view, setImageAlignment: 0i64];
    let tint: ObjcId = msg_send![class!(NSColor), secondaryLabelColor];
    send_void![image_view, setContentTintColor: tint];
    send_void![content, addSubview: image_view];
}

unsafe fn settings_heading_font() -> ObjcId {
    let rounded: ObjcId = msg_send![
        class!(NSFont),
        fontWithName: ns_string("SF Pro Rounded Semibold"),
        size: 18.0f64,
    ];
    if rounded != NIL {
        rounded
    } else {
        msg_send![
            class!(NSFont),
            systemFontOfSize: 18.0f64,
            weight: 0.4f64,
        ]
    }
}

fn visibility_key(kind: &str, name: &str) -> String {
    format!("{VISIBILITY_KEY_PREFIX}.{kind}.{name}")
}

fn visibility_defaults() -> &'static Mutex<Vec<(String, usize)>> {
    SETTINGS_VISIBILITY.get_or_init(|| Mutex::new(Vec::new()))
}

unsafe fn is_visible(key: &str) -> bool {
    let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
    let value: ObjcId = msg_send![defaults, objectForKey: ns_string(key)];
    let stored: bool = msg_send![defaults, boolForKey: ns_string(key)];
    value == NIL || stored
}

fn pending_visibility() -> &'static Mutex<Vec<(String, bool)>> {
    SETTINGS_PENDING_VISIBILITY.get_or_init(|| Mutex::new(Vec::new()))
}

unsafe fn add_visibility_checkbox(parent: ObjcId, title: &str, key: String, x: f64, y: f64) {
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    let button: ObjcId = Retained::into_raw(NSButton::buttonWithTitle_target_action(
        &*NSString::from_str(title),
        None,
        None,
        mtm,
    ))
    .cast();
    send_void![button, setButtonType: 3i64];
    let checked = pending_visibility()
        .lock()
        .unwrap()
        .iter()
        .find(|(pending_key, _)| pending_key == &key)
        .map(|(_, checked)| *checked)
        .unwrap_or_else(|| is_visible(&key));
    send_void![button, setState: if checked { 1i64 } else { 0i64 }];
    send_void![button, setFrame: NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(380.0, 24.0)
    )];
    send_void![parent, addSubview: button];
    visibility_defaults()
        .lock()
        .unwrap()
        .push((key, button as usize));
}

unsafe fn add_visibility_tree(
    content: ObjcId,
    snapshot: Option<&AppSnapshot>,
    sort_order: i64,
) -> ObjcId {
    let document_height = snapshot
        .map(|snapshot| {
            snapshot
                .teams
                .iter()
                .map(|team| 1 + team.projects.len())
                .sum::<usize>()
                .max(1) as f64
                * 26.0
                + 8.0
        })
        .unwrap_or(40.0);
    let document: ObjcId = msg_send![class!(NSView), alloc];
    let document: ObjcId = msg_send![
        document,
        initWithFrame: NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(404.0, document_height)
        )
    ];
    if let Some(snapshot) = snapshot {
        let mut y = document_height - 32.0;
        for team in &snapshot.teams {
            add_visibility_checkbox(
                document,
                &team.name,
                visibility_key("team", &team.name),
                8.0,
                y,
            );
            y -= 26.0;
            let mut projects = team.projects.clone();
            if sort_order == SORT_ALPHABETICAL {
                projects.sort_by_key(|project| project.name.to_ascii_lowercase());
            } else {
                projects.sort_by_key(|project| std::cmp::Reverse(project.latest_deployment_at()));
            }
            for project in &projects {
                add_visibility_checkbox(
                    document,
                    &project.name,
                    visibility_key("project", &format!("{}::{}", team.name, project.name)),
                    28.0,
                    y,
                );
                y -= 26.0;
            }
        }
    } else {
        let label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("No projects loaded yet.")];
        send_void![label, setFrame: NSRect::new(
            NSPoint::new(8.0, 8.0),
            NSSize::new(380.0, 24.0)
        )];
        send_void![document, addSubview: label];
    }

    let container: ObjcId = msg_send![class!(NSVisualEffectView), alloc];
    let container: ObjcId = msg_send![
        container,
        initWithFrame: NSRect::new(
            NSPoint::new(56.0, 72.0),
            NSSize::new(420.0, 218.0)
        )
    ];
    send_void![container, setMaterial: 6i64];
    send_void![container, setBlendingMode: 0i64];
    send_void![container, setState: 1i64];
    send_void![container, setWantsLayer: Bool::YES];
    let container_layer: ObjcId = msg_send![container, layer];
    send_void![container_layer, setCornerRadius: 10.0f64];
    send_void![container_layer, setBorderWidth: 1.0f64];
    let border_color: ObjcId = msg_send![class!(NSColor), separatorColor];
    let border_cg_color: *mut CGColor =
        Retained::into_raw((&*(border_color as *const AnyObject as *const NSColor)).CGColor());
    send_void![container_layer, setBorderColor: border_cg_color];
    let tint: ObjcId = msg_send![class!(NSColor), controlAccentColor];
    let tint: ObjcId = msg_send![tint, colorWithAlphaComponent: 0.06f64];
    let tint_cg_color: *mut CGColor =
        Retained::into_raw((&*(tint as *const AnyObject as *const NSColor)).CGColor());
    send_void![container_layer, setBackgroundColor: tint_cg_color];
    send_void![content, addSubview: container];

    let scroll: ObjcId = msg_send![class!(NSScrollView), alloc];
    let scroll: ObjcId = msg_send![
        scroll,
        initWithFrame: NSRect::new(
            NSPoint::new(64.0, 80.0),
            NSSize::new(404.0, 202.0)
        )
    ];
    send_void![scroll, setHasVerticalScroller: Bool::YES];
    send_void![scroll, setBorderType: 0i64];
    send_void![scroll, setDrawsBackground: Bool::NO];
    send_void![scroll, setDocumentView: document];
    send_void![content, addSubview: scroll];
    let clip_view: ObjcId = msg_send![scroll, contentView];
    let clip_bounds: NSRect = msg_send![clip_view, bounds];
    let top_y = (document_height - clip_bounds.size.height).max(0.0);
    send_void![
        clip_view,
        scrollToPoint: NSPoint::new(0.0, top_y)
    ];
    send_void![scroll, reflectScrolledClipView: clip_view];
    pending_visibility().lock().unwrap().clear();
    scroll
}

fn show_settings_modal(controller: ObjcId) -> Result<()> {
    unsafe {
        let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
        let app = app();
        send_void![app, activateIgnoringOtherApps: Bool::YES];
        let initial_token = load_token()?;
        SETTINGS_ORIGINAL_TOKEN
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone_from(&initial_token);
        let snapshot = match cached_snapshot() {
            Some(snapshot) => Some(snapshot),
            None => match initial_token.as_deref() {
                Some(token) => {
                    let snapshot = VercelClient::new(token)?.fetch_snapshot()?;
                    cache_snapshot(snapshot.clone());
                    Some(snapshot)
                }
                None => None,
            },
        };
        let window: ObjcId = msg_send![class!(NSWindow), alloc];
        let window: ObjcId = msg_send![
            window,
            initWithContentRect: NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(500.0, 560.0),
            ),
            styleMask: (1u64 | 2u64 | 32768u64),
            backing: 2u64,
            defer: Bool::NO,
        ];
        send_void![window, setTitle: ns_string("Settings")];
        send_void![window, setTitlebarAppearsTransparent: Bool::YES];
        send_void![window, setTitleVisibility: 0i64];
        send_void![window, setReleasedWhenClosed: Bool::NO];
        send_void![window, setInitialFirstResponder: NIL];
        let close_button: ObjcId = msg_send![window, standardWindowButton: 2i64];
        send_void![close_button, setTarget: controller];
        send_void![close_button, setAction: sel!(settingsCancel:)];
        send_void![window, center];
        let content: ObjcId = msg_send![window, contentView];

        let background: ObjcId = msg_send![class!(NSVisualEffectView), alloc];
        let background: ObjcId = msg_send![
            background,
            initWithFrame: NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(500.0, 560.0)
            )
        ];
        send_void![background, setMaterial: 7i64];
        send_void![background, setBlendingMode: 0i64];
        send_void![background, setState: 1i64];
        send_void![background, setAutoresizingMask: 18u64];
        send_void![content, addSubview: background];

        visibility_defaults().lock().unwrap().clear();
        pending_visibility().lock().unwrap().clear();
        SETTINGS_SNAPSHOT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone_from(&snapshot);
        SETTINGS_CONTENT.store(content as usize, Ordering::Relaxed);
        add_settings_symbol(content, "person.crop.circle", "Vercel account", 499.0);
        let account_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Vercel Account")];
        send_void![account_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 498.0), NSSize::new(300.0, 24.0)
        )];
        let heading_font = settings_heading_font();
        send_void![account_label, setFont: heading_font];
        send_void![content, addSubview: account_label];
        let label: ObjcId = msg_send![class!(NSTextField), labelWithString: ns_string(
            "Vercel token (used to load teams and projects):"
        )];
        send_void![label, setFrame: NSRect::new(
            NSPoint::new(56.0, 472.0), NSSize::new(420.0, 20.0)
        )];
        let secondary_color: ObjcId = msg_send![class!(NSColor), secondaryLabelColor];
        send_void![label, setTextColor: secondary_color];
        send_void![content, addSubview: label];

        let text_field: ObjcId = msg_send![class!(NSSecureTextField), alloc];
        let text_field: ObjcId = msg_send![text_field, initWithFrame: NSRect::new(
            NSPoint::new(56.0, 428.0), NSSize::new(420.0, 36.0)
        )];
        send_void![text_field, setEditable: Bool::YES];
        send_void![text_field, setSelectable: Bool::YES];
        if let Some(token) = initial_token {
            send_void![text_field, setStringValue: ns_string(&token)];
        }
        send_void![content, addSubview: text_field];

        let clear: ObjcId = Retained::into_raw(NSButton::buttonWithTitle_target_action(
            &*NSString::from_str("Clear Token"),
            Some(&*(controller as *const AnyObject)),
            Some(sel!(settingsClear:)),
            mtm,
        ))
        .cast();
        send_void![clear, setFrame: NSRect::new(
            NSPoint::new(56.0, 388.0), NSSize::new(120.0, 28.0)
        )];
        let trash: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("trash"),
            accessibilityDescription: ns_string("Delete"),
        ];
        if trash != NIL {
            send_void![clear, setImage: trash];
            send_void![clear, setImagePosition: 2i64];
            send_void![clear, setImageHugsTitle: Bool::NO];
        }
        send_void![clear, setHasDestructiveAction: Bool::YES];
        send_void![content, addSubview: clear];
        let generate: ObjcId = Retained::into_raw(NSButton::buttonWithTitle_target_action(
            &*NSString::from_str("Generate Token"),
            Some(&*(controller as *const AnyObject)),
            Some(sel!(openTokenSettings:)),
            mtm,
        ))
        .cast();
        send_void![generate, setFrame: NSRect::new(
            NSPoint::new(336.0, 388.0), NSSize::new(140.0, 28.0)
        )];
        let external_link: ObjcId = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("arrow.up.right"),
            accessibilityDescription: ns_string("Open externally"),
        ];
        if external_link != NIL {
            send_void![generate, setImage: external_link];
            send_void![generate, setImagePosition: 3i64];
            send_void![generate, setImageHugsTitle: Bool::NO];
        }
        send_void![content, addSubview: generate];

        add_settings_symbol(content, "square.grid.2x2", "Projects display", 337.0);
        let display_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Projects Display")];
        send_void![display_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 336.0), NSSize::new(420.0, 24.0)
        )];
        send_void![display_label, setFont: heading_font];
        send_void![content, addSubview: display_label];
        let sort_label: ObjcId =
            msg_send![class!(NSTextField), labelWithString: ns_string("Projects sort order")];
        send_void![sort_label, setFrame: NSRect::new(
            NSPoint::new(56.0, 302.0), NSSize::new(160.0, 22.0)
        )];
        send_void![content, addSubview: sort_label];
        let sort_popup: ObjcId = msg_send![class!(NSPopUpButton), alloc];
        let sort_popup: ObjcId = msg_send![
            sort_popup,
            initWithFrame: NSRect::new(
                NSPoint::new(270.0, 302.0),
                NSSize::new(206.0, 28.0),
            ),
            pullsDown: Bool::NO,
        ];
        send_void![sort_popup, setTarget: controller];
        send_void![sort_popup, setAction: sel!(settingsSortChanged:)];
        send_void![sort_popup, addItemWithTitle: ns_string("Last deployment date")];
        send_void![sort_popup, addItemWithTitle: ns_string("Alphabetically")];
        send_void![sort_popup, selectItemAtIndex: project_sort_order()];
        send_void![content, addSubview: sort_popup];
        let tree = add_visibility_tree(content, snapshot.as_ref(), project_sort_order());
        SETTINGS_TREE.store(tree as usize, Ordering::Relaxed);

        let cancel: ObjcId = msg_send![
            class!(NSButton),
            buttonWithTitle: ns_string("Cancel"),
            target: controller,
            action: sel!(settingsCancel:),
        ];
        send_void![cancel, setFrame: NSRect::new(
            NSPoint::new(330.0, 20.0), NSSize::new(70.0, 32.0)
        )];
        let save: ObjcId = msg_send![
            class!(NSButton),
            buttonWithTitle: ns_string("Save"),
            target: controller,
            action: sel!(settingsSave:),
        ];
        send_void![save, setFrame: NSRect::new(
            NSPoint::new(414.0, 20.0), NSSize::new(70.0, 32.0)
        )];
        send_void![save, setKeyEquivalent: ns_string("\r")];
        send_void![save, setKeyEquivalentModifierMask: 0u64];
        send_void![cancel, setKeyEquivalent: ns_string("\u{1b}")];
        send_void![cancel, setKeyEquivalentModifierMask: 0u64];
        send_void![save, setBezelStyle: 1i64];
        send_void![content, addSubview: cancel];
        send_void![content, addSubview: save];
        SETTINGS_WINDOW.store(window as usize, Ordering::Relaxed);
        SETTINGS_FIELD.store(text_field as usize, Ordering::Relaxed);
        SETTINGS_SORT.store(sort_popup as usize, Ordering::Relaxed);
        SETTINGS_RESPONSE.store(0, Ordering::Relaxed);
        send_void![window, makeKeyAndOrderFront: NIL];
        let _: Bool = msg_send![window, makeFirstResponder: NIL];
        let _: i64 = msg_send![app, runModalForWindow: window];
        let response = SETTINGS_RESPONSE.load(Ordering::Relaxed) as i64;
        send_void![window, orderOut: NIL];
        SETTINGS_WINDOW.store(0, Ordering::Relaxed);
        SETTINGS_FIELD.store(0, Ordering::Relaxed);
        SETTINGS_SORT.store(0, Ordering::Relaxed);
        SETTINGS_CONTENT.store(0, Ordering::Relaxed);
        SETTINGS_TREE.store(0, Ordering::Relaxed);
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
    let app = app();
    send_void![app, activateIgnoringOtherApps: Bool::YES];

    let alert: ObjcId = msg_send![class!(NSAlert), new];
    send_void![alert, setMessageText: ns_string(title)];
    send_void![alert, setInformativeText: ns_string(message)];
    let _: ObjcId = msg_send![alert, addButtonWithTitle: ns_string("OK")];
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
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        send_void![
            defaults,
            setObject: ns_string(token),
            forKey: ns_string(TOKEN_DEFAULTS_KEY),
        ];
    }
    Ok(())
}

fn clear_token() -> Result<()> {
    let entry = token_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            unsafe {
                let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
                send_void![defaults, removeObjectForKey: ns_string(TOKEN_DEFAULTS_KEY)];
            }
            Ok(())
        }
        Err(err) => Err(err).context("Failed to clear saved token"),
    }
}

fn load_token_from_defaults() -> Result<Option<String>> {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: ObjcId = msg_send![defaults, objectForKey: ns_string(TOKEN_DEFAULTS_KEY)];
        if value == NIL {
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

unsafe fn ns_string(value: &str) -> ObjcId {
    objc2::rc::Retained::into_raw(NSString::from_str(value)) as ObjcId
}

unsafe fn ns_string_to_rust(value: ObjcId) -> String {
    let utf8: *const c_char = msg_send![value, UTF8String];
    if utf8.is_null() {
        String::new()
    } else {
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

unsafe fn new_controller() -> ObjcId {
    let class = menu_controller_class();
    let controller: ObjcId = msg_send![class, new];
    controller
}

fn menu_controller_class() -> &'static AnyClass {
    static CLASS: OnceCell<&'static AnyClass> = OnceCell::new();
    CLASS.get_or_init(|| unsafe {
        let superclass = class!(NSObject);
        let mut decl = ClassBuilder::new(c"DevTreeMenuController", superclass)
            .expect("Failed to declare DevTreeMenuController");
        decl.add_method(
            sel!(menuWillOpen:),
            menu_will_open as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(refreshMenu:),
            refresh_menu as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(autoRefresh:),
            auto_refresh as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(loadMenu:),
            load_menu as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(openSettings:),
            open_settings as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(openRepresentedUrl:),
            open_represented_url as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(openTokenSettings:),
            open_token_settings as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(quitApp:),
            quit_app as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(sel!(noop:), noop as extern "C" fn(ObjcId, Sel, ObjcId));
        decl.add_method(
            sel!(settingsSave:),
            settings_save as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(settingsSortChanged:),
            settings_sort_changed as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(settingsCancel:),
            settings_cancel as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.add_method(
            sel!(settingsClear:),
            settings_clear as extern "C" fn(ObjcId, Sel, ObjcId),
        );
        decl.register()
    })
}

extern "C" fn noop(_: ObjcId, _: Sel, _: ObjcId) {}

extern "C" fn settings_sort_changed(_: ObjcId, _: Sel, sender: ObjcId) {
    unsafe {
        let content = SETTINGS_CONTENT.load(Ordering::Relaxed) as ObjcId;
        let old_tree = SETTINGS_TREE.load(Ordering::Relaxed) as ObjcId;
        if content == NIL {
            return;
        }
        if old_tree != NIL {
            send_void![old_tree, removeFromSuperview];
        }
        {
            let controls = visibility_defaults().lock().unwrap();
            let mut pending = pending_visibility().lock().unwrap();
            pending.extend(controls.iter().map(|(key, button)| {
                let button = *button as ObjcId;
                let state: i64 = msg_send![button, state];
                (key.clone(), state == 1)
            }));
        }
        visibility_defaults().lock().unwrap().clear();
        let sort: i64 = msg_send![sender, indexOfSelectedItem];
        let snapshot = SETTINGS_SNAPSHOT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let tree = add_visibility_tree(content, snapshot.as_ref(), sort);
        SETTINGS_TREE.store(tree as usize, Ordering::Relaxed);
    }
}

extern "C" fn settings_save(_: ObjcId, _: Sel, _: ObjcId) {
    unsafe {
        let field = SETTINGS_FIELD.load(Ordering::Relaxed) as ObjcId;
        let token = normalize_token(&ns_string_to_rust(msg_send![field, stringValue]));
        let original_token = SETTINGS_ORIGINAL_TOKEN
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let new_token = (!token.is_empty()).then_some(token.as_str());
        let token_changed = original_token.as_deref() != new_token;
        let result = if token.is_empty() && token_changed {
            clear_token()
        } else if token.is_empty() {
            Ok(())
        } else if original_token.as_deref() == Some(token.as_str()) {
            Ok(())
        } else {
            VercelClient::new(&token)
                .and_then(|client| client.validate_token())
                .context("The provided Vercel token is invalid")
                .and_then(|_| save_token(&token))
        };
        match result {
            Ok(()) => {
                let sort_popup = SETTINGS_SORT.load(Ordering::Relaxed) as ObjcId;
                let sort: i64 = msg_send![sort_popup, indexOfSelectedItem];
                set_project_sort_order(sort);
                let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
                for (key, button) in visibility_defaults().lock().unwrap().iter() {
                    let button = *button as ObjcId;
                    let state: i64 = msg_send![button, state];
                    send_void![
                        defaults,
                        setBool: state == 1i64,
                        forKey: ns_string(key),
                    ];
                }
                let _: Bool = msg_send![defaults, synchronize];
                if token_changed {
                    clear_snapshot_cache();
                }
                SETTINGS_RESPONSE.store(SETTINGS_SAVE as usize, Ordering::Relaxed);
                send_void![app(), stopModal];
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

extern "C" fn settings_cancel(_: ObjcId, _: Sel, _: ObjcId) {
    SETTINGS_RESPONSE.store(0, Ordering::Relaxed);
    unsafe {
        send_void![app(), stopModal];
    }
}

extern "C" fn settings_clear(_: ObjcId, _: Sel, _: ObjcId) {
    SETTINGS_RESPONSE.store(SETTINGS_CLEAR as usize, Ordering::Relaxed);
    unsafe {
        send_void![app(), stopModal];
    }
}

fn project_sort_order() -> i64 {
    unsafe {
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
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
        let defaults: ObjcId = msg_send![class!(NSUserDefaults), standardUserDefaults];
        send_void![
            defaults,
            setInteger: order,
            forKey: ns_string(PROJECT_SORT_KEY),
        ];
        let _: Bool = msg_send![defaults, synchronize];
    }
}
