//! Lyra service names and trust levels.
pub(crate) const NETWORKING: &str = "00150323";
pub(crate) const CAST: &str = "com.xiaomi.mirror:cast";
pub(crate) const CAMERA_METADATA: &str = "com.milink.service:distributedHardware";
pub(crate) const CAMERA_COMMAND: &str = "com.milink.service:cameraCommand";
pub(crate) const CAMERA_P2P: &str = "com.milink.service:cameraP2P";
pub(crate) const CLIPBOARD_PHONE: &str = "com.milink.service:universalClipboard";
pub(crate) const CLIPBOARD_LOCAL: &str = "pcmanager:universalClipboard";
pub(crate) const RECENT_APPS: &str = "com.xiaomi.mirror:RecentApps";
pub(crate) const MISHARE_BASIC: &str = "com.miui.mishare.connectivity:miShareBasic";
pub(crate) const MISHARE_TRANSFER: &str = "com.miui.mishare.connectivity:miLyraShareTransfer";
pub(crate) const PC_MISHARE_BASIC: &str = "pcmanager:miShareBasic";
pub(crate) const PC_MISHARE_TRANSFER: &str = "pcmanager:miLyraShareTransfer";

pub(crate) const TRUST_PAIR: u32 = 16;
pub(crate) const TRUST_MISHARE: u32 = 48;
