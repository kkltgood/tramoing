//! 不依赖图形渲染器的原生提示，供启动失败和独立崩溃报告使用。
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSApplication};
use objc2_foundation::NSString;

pub fn show(title: &str, detail: &str, confirm: &str, cancel: Option<&str>) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str(confirm));
    if let Some(cancel) = cancel {
        alert
            .addButtonWithTitle(&NSString::from_str(cancel))
            .setKeyEquivalent(&NSString::from_str("\u{1b}"));
    }
    alert.runModal() == NSAlertFirstButtonReturn
}
