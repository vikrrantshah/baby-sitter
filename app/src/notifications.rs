#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationAccess {
    Unbundled,
    NotDetermined,
    Denied,
    Allowed {
        banners: bool,
        notification_center: bool,
    },
}

#[cfg(target_os = "macos")]
use {
    block2::{DynBlock, RcBlock},
    objc2::{
        AllocAnyThread, define_class, msg_send,
        rc::{Retained, autoreleasepool},
        runtime::{Bool, ProtocolObject},
    },
    objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString},
    objc2_user_notifications::{
        UNAlertStyle, UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent,
        UNNotification, UNNotificationPresentationOptions, UNNotificationRequest,
        UNNotificationSetting, UNNotificationSettings, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    },
    std::{
        ptr::NonNull,
        sync::{Once, mpsc},
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
};

#[cfg(target_os = "macos")]
define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class has no ivars or Drop.
    #[unsafe(super = NSObject)]
    struct NotificationDelegate;

    // SAFETY: NSObjectProtocol has no additional requirements.
    unsafe impl NSObjectProtocol for NotificationDelegate {}

    // SAFETY: The delegate implements the declared method with its required signature.
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present_notification(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);

#[cfg(target_os = "macos")]
pub fn init() {
    autoreleasepool(|_| {
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return;
        }

        static DELEGATE: Once = Once::new();
        DELEGATE.call_once(|| {
            // SAFETY: NSObject's init method initializes the allocated subclass.
            let delegate: Retained<NotificationDelegate> =
                unsafe { msg_send![super(NotificationDelegate::alloc().set_ivars(())), init] };
            UNUserNotificationCenter::currentNotificationCenter()
                .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // The center holds a weak reference, so retain this delegate for the process lifetime.
            let _ = Retained::into_raw(delegate);
        });

        request_authorization_with_completion(RcBlock::new(|_: Bool, _: *mut NSError| {}));
    });
}

#[cfg(target_os = "macos")]
fn request_authorization_with_completion(completion: RcBlock<dyn Fn(Bool, *mut NSError)>) {
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &completion,
        );
}

#[cfg(target_os = "macos")]
pub fn request_authorization() {
    autoreleasepool(|_| {
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        request_authorization_with_completion(RcBlock::new(move |_: Bool, _: *mut NSError| {
            let _ = tx.send(());
        }));
        let _ = rx.recv();
    });
}

#[cfg(target_os = "macos")]
pub fn access() -> Result<NotificationAccess, &'static str> {
    autoreleasepool(|_| {
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return Ok(NotificationAccess::Unbundled);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let completion = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: The framework supplies a non-null settings object valid for this callback.
            let settings = unsafe { settings.as_ref() };
            let access = match settings.authorizationStatus() {
                UNAuthorizationStatus::NotDetermined => Ok(NotificationAccess::NotDetermined),
                UNAuthorizationStatus::Denied => Ok(NotificationAccess::Denied),
                UNAuthorizationStatus::Authorized
                | UNAuthorizationStatus::Provisional
                | UNAuthorizationStatus::Ephemeral => Ok(NotificationAccess::Allowed {
                    banners: settings.alertStyle() != UNAlertStyle::None
                        && settings.alertSetting() == UNNotificationSetting::Enabled,
                    notification_center: settings.notificationCenterSetting()
                        == UNNotificationSetting::Enabled,
                }),
                _ => Err("Unknown macOS notification authorization status"),
            };
            let _ = tx.send(access);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .getNotificationSettingsWithCompletionHandler(&completion);
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or(Err("macOS did not answer"))
    })
}

#[cfg(not(target_os = "macos"))]
pub fn request_authorization() {}

#[cfg(not(target_os = "macos"))]
pub fn access() -> Result<NotificationAccess, &'static str> {
    Ok(NotificationAccess::Unbundled)
}

#[cfg(target_os = "macos")]
pub fn post(title: &str, body: &str) {
    autoreleasepool(|_| {
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return;
        }

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let identifier = NSString::from_str(&format!("{title}-{timestamp}"));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &identifier,
            &content,
            None,
        );
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, None);
    });
}

#[cfg(not(target_os = "macos"))]
pub fn init() {}

#[cfg(not(target_os = "macos"))]
pub fn post(_title: &str, _body: &str) {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn unbundled_notifications_are_safe_from_the_runner_thread() {
        assert!(objc2::rc::autoreleasepool(|_| {
            objc2_foundation::NSBundle::mainBundle()
                .bundleIdentifier()
                .is_none()
        }));

        super::init();
        std::thread::spawn(|| {
            super::post("owner/repo#42: Merged", "Pull request merged");
        })
        .join()
        .unwrap();
    }
}
