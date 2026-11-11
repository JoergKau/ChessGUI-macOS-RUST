//! macOS About panel customization for SiliconBoard.
//!
//! winit installs the standard application menu. Its **About SiliconBoard** item
//! calls `orderFrontStandardAboutPanel:` on `NSApplication`, which builds the
//! panel from the bundle's `Info.plist`.
//!
//! AppKit has no setter for panel options on macOS — only
//! `orderFrontStandardAboutPanelWithOptions:`. We therefore swizzle the
//! no-options method: when the menu item fires, our implementation shows the
//! same standard panel with explicit options. Passing the name, version and
//! runtime icon explicitly keeps the panel correct even when the binary runs
//! outside a `.app` bundle (`cargo run`), where there is no `Info.plist`.
//! The credits block adds the project lines; it also carries the copyright
//! when no bundle can supply `NSHumanReadableCopyright`.
//!
//! The module is macOS-only and deliberately isolated: no other part of the
//! app depends on Objective-C.

/// Project page shown in the About panel.
const PROJECT_PAGE: &str = "https://github.com/JoergKau";

/// Prefix before the URL in the credits block (used for the link range).
const CREDITS_PREFIX: &str = "A UCI chess GUI for Apple Silicon\nProject Page: ";

/// Copyright shown in the credits when no `.app` bundle provides one.
const COPYRIGHT: &str = "Copyright © 2026 JoKa. MIT License.";

/// Application name shown in the About panel.
const APP_NAME: &str = "SiliconBoard";

/// Application version shown in the About panel.
/// Shown as "SiliconBoard 1.2.1" in the About panel.
const APP_VERSION: &str = "1.2.1";

/// Credits block: two project lines, plus the copyright when the binary runs
/// without a bundle (`cargo run`), where `Info.plist` cannot supply it.
fn credits_text(with_copyright: bool) -> String {
    // Blank line under the version; the standard About panel otherwise places
    // the credits directly beneath it.
    let mut text = String::from("\n");
    text.push_str(CREDITS_PREFIX);
    text.push_str(PROJECT_PAGE);
    if with_copyright {
        // Blank line between the project link and the copyright, matching the
        // gap the standard About panel leaves before its copyright field.
        text.push_str("\n\n");
        text.push_str(COPYRIGHT);
    }
    text
}

/// Installs the About panel credits. Returns `true` on success.
#[cfg(target_os = "macos")]
pub fn install() -> bool {
    imp::install()
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::ffi::{class_getInstanceMethod, method_setImplementation};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Method, Sel};
    use objc2::{class, msg_send, Message};
    use objc2_app_kit::{
        NSAboutPanelOptionApplicationIcon, NSAboutPanelOptionApplicationName,
        NSAboutPanelOptionCredits, NSAboutPanelOptionVersion, NSImage, NSMutableParagraphStyle,
        NSTextAlignment,
    };
    use objc2_foundation::{
        NSBundle, NSDictionary, NSMutableAttributedString, NSRange, NSString, NSURL,
    };

    pub fn install() -> bool {
        // SAFETY: main thread, NSApplication is a singleton. We replace one
        // method implementation; the replacement calls the options variant.
        unsafe {
            let cls = class!(NSApplication) as *const AnyClass as *mut AnyClass;
            let method: *const Method =
                class_getInstanceMethod(cls, Sel::register(c"orderFrontStandardAboutPanel:"));
            if method.is_null() {
                log::warn!("NSApplication.orderFrontStandardAboutPanel: not found");
                return false;
            }
            let imp: Imp = std::mem::transmute(order_front_about as *const ());
            method_setImplementation(method, imp);
        }
        log::info!("installed About panel credits");
        true
    }

    /// True when the process runs from a `.app` bundle with an `Info.plist`.
    fn has_bundle() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    /// Casts any Objective-C object reference to `AnyObject`.
    unsafe fn as_any<T: Message>(object: &T) -> &AnyObject {
        unsafe { &*(object as *const T).cast::<AnyObject>() }
    }

    /// Builds the credits text with a clickable link on the project URL.
    ///
    /// `NSLinkAttributeName` is not exposed by objc2-foundation; its raw
    /// value is the string `NSLink`.
    pub(super) fn attributed_credits() -> Retained<NSMutableAttributedString> {
        let text_value = super::credits_text(!has_bundle());
        let text = NSString::from_str(&text_value);
        let credits = NSMutableAttributedString::from_nsstring(&text);

        // Center the whole block and give each line (notably the copyright)
        // breathing room; the options variant otherwise uses tight,
        // left-aligned paragraphs.
        let style = NSMutableParagraphStyle::new();
        style.setAlignment(NSTextAlignment::Center);
        style.setParagraphSpacing(4.0);
        let style_key = NSString::from_str("NSParagraphStyle");
        let full_range = NSRange::new(0, text_value.encode_utf16().count());
        // SAFETY: `NSParagraphStyle` expects an NSParagraphStyle value; the
        // range covers the whole UTF-16 string.
        unsafe {
            credits.addAttribute_value_range(&style_key, as_any(&*style), full_range);
        }

        let link_key = NSString::from_str("NSLink");
        let url_text = NSString::from_str(super::PROJECT_PAGE);
        if let Some(url) = NSURL::URLWithString(&url_text) {
            // The dictionary/attribute APIs store values as `AnyObject`.
            let url_object: &AnyObject = unsafe { as_any(&*url) };
            // The link range follows the actual URL position, which shifts
            // when the bare panel adds its leading blank line.
            let link_start = text_value
                .find(super::PROJECT_PAGE)
                .map(|byte| text_value[..byte].encode_utf16().count())
                .unwrap_or(0);
            let range = NSRange::new(link_start, super::PROJECT_PAGE.encode_utf16().count());
            // SAFETY: `NSLink` expects an NSURL value; the range is the exact
            // UTF-16 span of the URL inside the credits string.
            unsafe {
                credits.addAttribute_value_range(&link_key, url_object, range);
            }
        }
        credits
    }

    /// Replacement for `orderFrontStandardAboutPanel:`.
    ///
    /// Shows the standard panel with explicit name, version, runtime icon and
    /// credits. Supplying the icon matters outside a bundle: the options
    /// variant otherwise ignores `NSApplication.applicationIconImage`.
    unsafe extern "C-unwind" fn order_front_about(this: *mut AnyObject, _cmd: Sel) {
        log::info!("showing About panel with credits");

        let name = NSString::from_str(super::APP_NAME);
        let version = NSString::from_str(super::APP_VERSION);
        let credits = attributed_credits();
        // eframe sets the runtime icon from `ViewportBuilder::with_icon`.
        let icon: Option<Retained<NSImage>> = unsafe { msg_send![this, applicationIconImage] };
        let bundled = has_bundle();

        let mut keys: Vec<&NSString> = Vec::new();
        let mut values: Vec<&AnyObject> = Vec::new();
        // SAFETY: all keys are AppKit constants and all values are the types
        // those keys expect (NSString, NSAttributedString, NSImage).
        unsafe {
            keys.push(NSAboutPanelOptionApplicationName);
            values.push(as_any(&*name));
            // Inside a bundle, keep the version strings from Info.plist
            // (short version + build number); outside, supply them explicitly.
            if !bundled {
                keys.push(NSAboutPanelOptionVersion);
                values.push(as_any(&*version));
            }
            keys.push(NSAboutPanelOptionCredits);
            values.push(as_any(&*credits));
            if let Some(icon) = icon.as_deref() {
                keys.push(NSAboutPanelOptionApplicationIcon);
                values.push(as_any(icon));
            }
        }
        let options = NSDictionary::from_slices(&keys, &values);

        let _: () = msg_send![
            this,
            orderFrontStandardAboutPanelWithOptions: &*options
        ];
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn credits_contain_the_project_page() {
        let text = super::credits_text(false);
        assert!(text.contains("Project Page: https://github.com/JoergKau"));
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn bundled_and_unbundled_credits_differ_only_by_the_copyright() {
        let bundled = super::credits_text(false);
        let unbundled = super::credits_text(true);
        assert!(unbundled.trim_start().starts_with(bundled.trim_start()));
        assert!(unbundled.ends_with("Copyright © 2026 JoKa. MIT License."));
        assert_eq!(unbundled.lines().count(), 5);
    }

    /// The URL part must carry the `NSLink` attribute (clickable); the rest
    /// of the credits must not.
    #[cfg(target_os = "macos")]
    #[test]
    fn project_page_is_a_clickable_link() {
        let credits = super::imp::attributed_credits();
        let text = credits.string().to_string();
        let url_index = text
            .find(super::PROJECT_PAGE)
            .expect("credits must contain the project page");
        let at_url =
            unsafe { credits.attributesAtIndex_effectiveRange(url_index, std::ptr::null_mut()) };
        let at_text = unsafe { credits.attributesAtIndex_effectiveRange(0, std::ptr::null_mut()) };
        assert!(
            at_url.len() > at_text.len(),
            "expected a link attribute on the URL range"
        );
    }
}
