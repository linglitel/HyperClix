//! Screen input key mapping and SDK input messages.
use super::Screen;
use crate::{
    Result,
    protocol::messages::mirror::{KeyInput, MouseInput, ScreenCommand, TextInput},
};
use anyhow::bail;

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum MouseAction {
    LeftDown = 0,
    LeftUp = 1,
    RightDown = 2,
    RightUp = 3,
    LeftDoubleClick = 4,
    Move = 5,
    WheelForward = 6,
    WheelBackward = 7,
    WheelLeft = 8,
    WheelRight = 9,
}

impl Screen {
    pub async fn resize(&self, width: u32, height: u32, density: u32) -> Result<()> {
        if width == 0 || height == 0 || density == 0 {
            bail!("尺寸和 density 必须大于 0");
        }
        let cfg = self.configuration();
        let request = if let Some(package) = &self.inner.package {
            ScreenCommand::App {
                package: package.clone(),
                width,
                height,
                density,
                screen_id: cfg.screen_id,
                resize: true,
                prefer_phone_ime: false,
            }
        } else {
            ScreenCommand::Resize {
                session_id: cfg.session_id,
                screen_id: cfg.screen_id,
                width,
                height,
                density,
            }
        };
        self.inner.channel.send(&request.encode()).await
    }

    /// Commit text after local IME composition; preedit stays on the computer.
    pub async fn commit_text(&self, text: &str) -> Result<()> {
        let cfg = self.configuration();
        self.inner
            .channel
            .send(
                &TextInput {
                    session_id: cfg.session_id,
                    screen_id: cfg.screen_id,
                    text: text.to_owned(),
                }
                .encode(),
            )
            .await
    }

    /// Coordinates are phone canvas pixels, before UI display scaling.
    pub async fn mouse(
        &self,
        action: MouseAction,
        x: i32,
        y: i32,
        state: u32,
        scroll_delta: i32,
    ) -> Result<()> {
        let cfg = self.configuration();
        self.inner
            .channel
            .send(
                &MouseInput {
                    session_id: cfg.session_id,
                    screen_id: cfg.screen_id,
                    action: action as u32,
                    state,
                    x,
                    y,
                    scroll: scroll_delta,
                }
                .encode(),
            )
            .await
    }

    pub async fn key(&self, code: u32, down: bool, meta_info: u32) -> Result<()> {
        let cfg = self.configuration();
        self.inner
            .channel
            .send(
                &KeyInput {
                    session_id: cfg.session_id,
                    screen_id: cfg.screen_id,
                    code,
                    meta: meta_info,
                    down,
                }
                .encode(),
            )
            .await
    }
}

/// GDK key names mapped to Android codes. Text composition uses `commit_text`.
pub fn android_key(name: &str) -> Option<(u32, bool)> {
    let mut shift = false;
    let normalized = match name {
        "exclam" => {
            shift = true;
            "1"
        }
        "at" => {
            shift = true;
            "2"
        }
        "numbersign" => {
            shift = true;
            "3"
        }
        "dollar" => {
            shift = true;
            "4"
        }
        "percent" => {
            shift = true;
            "5"
        }
        "asciicircum" => {
            shift = true;
            "6"
        }
        "ampersand" => {
            shift = true;
            "7"
        }
        "asterisk" => {
            shift = true;
            "8"
        }
        "parenleft" => {
            shift = true;
            "9"
        }
        "parenright" => {
            shift = true;
            "0"
        }
        "underscore" => {
            shift = true;
            "minus"
        }
        "plus" => {
            shift = true;
            "equal"
        }
        "colon" => {
            shift = true;
            "semicolon"
        }
        "quotedbl" => {
            shift = true;
            "apostrophe"
        }
        "less" => {
            shift = true;
            "comma"
        }
        "greater" => {
            shift = true;
            "period"
        }
        "question" => {
            shift = true;
            "slash"
        }
        "bar" => {
            shift = true;
            "backslash"
        }
        "braceleft" => {
            shift = true;
            "bracketleft"
        }
        "braceright" => {
            shift = true;
            "bracketright"
        }
        "asciitilde" => {
            shift = true;
            "grave"
        }
        "ISO_Left_Tab" => {
            shift = true;
            "Tab"
        }
        value => value,
    };
    if normalized.len() == 1 {
        let ch = normalized.as_bytes()[0];
        if ch.is_ascii_alphabetic() {
            return Some((
                29 + ch.to_ascii_lowercase() as u32 - 'a' as u32,
                shift || ch.is_ascii_uppercase(),
            ));
        }
        if ch.is_ascii_digit() {
            return Some((7 + ch as u32 - '0' as u32, shift));
        }
    }
    let code = match normalized {
        "Home" => 3,
        "Escape" | "Back" => 4,
        "AppSwitch" => 187,
        "Return" | "Enter" => 66,
        "BackSpace" => 67,
        "Delete" => 112,
        "Tab" => 61,
        "space" | "Space" => 62,
        "Left" => 21,
        "Right" => 22,
        "Up" => 19,
        "Down" => 20,
        "Page_Up" => 92,
        "Page_Down" => 93,
        "End" => 123,
        "MoveHome" => 122,
        "Insert" => 124,
        "Shift_L" => 59,
        "Shift_R" => 60,
        "Control_L" => 113,
        "Control_R" => 114,
        "Alt_L" => 57,
        "Alt_R" => 58,
        "Meta_R" | "Super_R" => 118,
        "Caps_Lock" => 115,
        "Num_Lock" => 143,
        "Scroll_Lock" => 116,
        "Print" => 120,
        "Pause" => 121,
        "comma" => 55,
        "period" => 56,
        "grave" => 68,
        "minus" => 69,
        "equal" => 70,
        "bracketleft" => 71,
        "bracketright" => 72,
        "backslash" => 73,
        "semicolon" => 74,
        "apostrophe" => 75,
        "slash" => 76,
        "KP_Add" => 157,
        "KP_Subtract" => 156,
        "KP_Multiply" => 155,
        "KP_Divide" => 154,
        "KP_Decimal" => 158,
        "KP_Separator" => 159,
        "KP_Enter" => 160,
        "KP_Equal" => 161,
        "XF86AudioPlay" => 85,
        _ => {
            if let Some(n) = normalized
                .strip_prefix('F')
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| (1..=12).contains(n))
            {
                130 + n
            } else if let Some(n) = normalized
                .strip_prefix("KP_")
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| *n <= 9)
            {
                144 + n
            } else {
                return None;
            }
        }
    };
    Some((code, shift))
}
