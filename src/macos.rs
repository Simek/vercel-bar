#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSStatusBar, NSVariableStatusItemLength,
};
use objc2_foundation::{NSAutoreleasePool, NSString};
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

use crate::settings::{
    SORT_ALPHABETICAL, VERCEL_TOKEN_SETTINGS_URL, auto_refresh_minutes, is_visible, load_token,
    project_sort_order, settings_cancel, settings_clear, settings_save, settings_sort_changed,
    show_settings_modal, visibility_key,
};
use crate::vercel::{AppSnapshot, ProjectSnapshot, TeamSnapshot, VercelClient};

pub(crate) type ObjcId = *mut AnyObject;
pub(crate) const NIL: ObjcId = std::ptr::null_mut();

const COMMAND_MODIFIER_MASK: u64 = 1 << 20;

static SNAPSHOT_CACHE: OnceCell<Mutex<Option<(Instant, AppSnapshot)>>> = OnceCell::new();
static STATUS_ITEM: AtomicUsize = AtomicUsize::new(0);
static APP_MENU: AtomicUsize = AtomicUsize::new(0);

macro_rules! send_void {
    ($receiver:expr, $($message:tt)*) => {{
        let _: () = msg_send![$receiver, $($message)*];
    }};
}

struct AppLifetime {
    _controller: ObjcId,
    _status_item: ObjcId,
}

pub(crate) fn app() -> ObjcId {
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
        set_status_icon(button, "arrow.3.trianglepath");

        let menu = new_menu();
        send_void![menu, setAutoenablesItems: Bool::NO];
        send_void![menu, setDelegate: controller];
        populate_loading_menu(menu, controller);
        send_void![status_item, setMenu: menu];

        APP_MENU.store(menu as usize, Ordering::Relaxed);

        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.05f64,
            target: controller,
            selector: sel!(loadMenu:),
            userInfo: menu,
            repeats: Bool::NO,
        ];
        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: auto_refresh_minutes() as f64 * 60.0,
            target: controller,
            selector: sel!(autoRefresh:),
            userInfo: NIL,
            repeats: Bool::NO,
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

extern "C" fn redeploy_production(this: ObjcId, _: Sel, sender: ObjcId) {
    unsafe {
        let represented: ObjcId = msg_send![sender, representedObject];
        if represented == NIL {
            return;
        }
        let parts = ns_string_to_rust(represented);
        let mut fields = parts.splitn(3, '|');
        let Some(deployment_id) = fields.next().filter(|value| !value.is_empty()) else {
            show_error_alert(
                "Failed to redeploy production",
                "The deployment ID is missing.",
            );
            return;
        };
        let team_id = fields.next().filter(|value| !value.is_empty());
        let Some(project_name) = fields.next().filter(|value| !value.is_empty()) else {
            show_error_alert(
                "Failed to redeploy production",
                "The project name is missing.",
            );
            return;
        };
        let alert: ObjcId = msg_send![class!(NSAlert), new];
        let app_icon: ObjcId = msg_send![class!(NSImage), imageNamed: ns_string("VercelBar")];
        if app_icon != NIL {
            send_void![alert, setIcon: app_icon];
        }
        send_void![alert, setMessageText: ns_string("Redeploy production?")];
        send_void![
            alert,
            setInformativeText: ns_string(
                "This will trigger a new build of the current production deployment."
            )
        ];
        let _: ObjcId = msg_send![alert, addButtonWithTitle: ns_string("Redeploy")];
        let _: ObjcId = msg_send![alert, addButtonWithTitle: ns_string("Cancel")];
        let response: i64 = msg_send![alert, runModal];
        if response != 1000 {
            return;
        }

        send_void![alert, setMessageText: ns_string("Redeploying production…")];
        send_void![
            alert,
            setInformativeText: ns_string("Waiting for Vercel to start the redeployment.")
        ];
        let loading_button: ObjcId = msg_send![alert, buttons];
        let loading_button: ObjcId = msg_send![loading_button, objectAtIndex: 0usize];
        send_void![loading_button, setTitle: ns_string("Redeploying…")];
        send_void![loading_button, setEnabled: Bool::NO];
        let cancel_button: ObjcId = msg_send![alert, buttons];
        let cancel_button: ObjcId = msg_send![cancel_button, objectAtIndex: 1usize];
        send_void![cancel_button, setHidden: Bool::YES];
        let result = load_token()
            .and_then(|token| {
                token.map_or_else(
                    || Err(anyhow::anyhow!("No Vercel token configured")),
                    |token| VercelClient::new(token),
                )
            })
            .and_then(|client| client.redeploy(deployment_id, project_name, team_id));
        let (title, message) = match &result {
            Ok(()) => (
                "Production redeploy started",
                "Vercel is redeploying the current production deployment.".to_owned(),
            ),
            Err(err) => ("Failed to redeploy production", err.to_string()),
        };
        send_void![alert, setMessageText: ns_string(title)];
        send_void![alert, setInformativeText: ns_string(&message)];
        send_void![loading_button, setTitle: ns_string("OK")];
        send_void![loading_button, setEnabled: Bool::YES];
        let _: i64 = msg_send![alert, runModal];
        if result.is_ok() {
            clear_snapshot_cache();
            let menu = APP_MENU.load(Ordering::Relaxed) as ObjcId;
            if menu != NIL {
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
    unsafe {
        let menu = APP_MENU.load(Ordering::Relaxed) as ObjcId;
        if menu != NIL && cached_snapshot().is_none() {
            rebuild_menu(menu, this);
        }
        let _: ObjcId = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: auto_refresh_minutes() as f64 * 60.0,
            target: this,
            selector: sel!(autoRefresh:),
            userInfo: NIL,
            repeats: Bool::NO,
        ];
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
        set_menu_item_image(item, image);
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
        set_menu_item_image(project_item, status_image);
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
        let configured = if configured == NIL { image } else { configured };
        send_void![configured, setTemplate: Bool::NO];
        configured
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
        add_url_item(
            project_menu,
            "Open production logs",
            &project.logs_url,
            controller,
        );
    }

    add_separator(project_menu);
    let mut has_deployment_actions = false;
    if let Some(url) = &project.latest_deployment_url {
        add_url_item(project_menu, "Open latest deployment", url, controller);
        has_deployment_actions = true;
    }
    if let Some(deployment_id) = &project.latest_deployment_id {
        let represented = format!(
            "{}|{}|{}",
            deployment_id,
            project.team_id.as_deref().unwrap_or_default(),
            project.name
        );
        add_represented_action_item(
            project_menu,
            "Redeploy production",
            &represented,
            sel!(redeployProduction:),
            controller,
            !matches!(
                project.latest_status,
                crate::vercel::DeploymentStatus::Pending
            ),
        );
        has_deployment_actions = true;
    }
    if has_deployment_actions {
        add_separator(project_menu);
    }
    add_menu_header(project_menu, "Deployment information");
    add_deployment_item(project_menu, &project.latest_status_label);
    if let Some(page_views) = project.page_views.filter(|&page_views| page_views > 0) {
        add_page_views_item(project_menu, page_views);
    }

    send_void![project_item, setSubmenu: project_menu];
    send_void![menu, addItem: project_item];
}

unsafe fn set_menu_item_image(item: ObjcId, image: ObjcId) {
    send_void![item, setImage: image];
    send_void![item, setPreferredImageVisibility: 1i64];
}

pub(crate) fn cache_snapshot(snapshot: AppSnapshot) {
    let cache = SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    *cache.lock().unwrap() = Some((Instant::now(), snapshot));
}

pub(crate) fn clear_snapshot_cache() {
    if let Some(cache) = SNAPSHOT_CACHE.get() {
        *cache.lock().unwrap() = None;
    }
}

pub(crate) fn cached_snapshot() -> Option<AppSnapshot> {
    let cache = SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    let guard = cache.lock().unwrap();
    guard
        .as_ref()
        .filter(|(time, _)| time.elapsed() < Duration::from_secs(auto_refresh_minutes() * 60))
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
            set_menu_item_image(item, image);
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
        set_menu_item_image(item, image);
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
        set_menu_item_image(item, image);
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
        "Refresh" => Some("arrow.trianglehead.2.clockwise"),
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
            set_menu_item_image(item, image);
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

unsafe fn add_represented_action_item(
    menu: ObjcId,
    title: &str,
    represented: &str,
    action: Sel,
    controller: ObjcId,
    enabled: bool,
) {
    let item: ObjcId = msg_send![class!(NSMenuItem), alloc];
    let item: ObjcId = msg_send![
        item,
        initWithTitle: ns_string(title),
        action: action,
        keyEquivalent: ns_string(""),
    ];
    send_void![item, setTarget: controller];
    send_void![item, setRepresentedObject: ns_string(represented)];
    send_void![
        item,
        setEnabled: if enabled { Bool::YES } else { Bool::NO }
    ];
    let image: ObjcId = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string("arrow.uturn.right"),
        accessibilityDescription: ns_string(title),
    ];
    if image != NIL {
        set_menu_item_image(item, image);
    }
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

pub(crate) unsafe fn show_error_alert(title: &str, message: &str) {
    let app = app();
    send_void![app, activateIgnoringOtherApps: Bool::YES];

    let alert: ObjcId = msg_send![class!(NSAlert), new];
    send_void![alert, setMessageText: ns_string(title)];
    send_void![alert, setInformativeText: ns_string(message)];
    let _: ObjcId = msg_send![alert, addButtonWithTitle: ns_string("OK")];
    let _: i64 = msg_send![alert, runModal];
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

pub(crate) unsafe fn ns_string(value: &str) -> ObjcId {
    Retained::into_raw(NSString::from_str(value)) as ObjcId
}

pub(crate) unsafe fn ns_string_to_rust(value: ObjcId) -> String {
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
            sel!(redeployProduction:),
            redeploy_production as extern "C" fn(ObjcId, Sel, ObjcId),
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
