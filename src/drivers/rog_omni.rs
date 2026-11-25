//! ASUS ROG OMNI RECEIVER (VID 0B05, PID 1ACE): one 2.4 GHz receiver shared by a ROG mouse and a
//! ROG keyboard (issue #428: ROG Harpe Ace Aim Lab Edition and ROG Azoth X).
//!
//! Protocol as used by G-Helper (GPL-3.0, github.com/seerge/g-helper: app/Peripherals/
//! PeripheralsProvider.cs DedectOmniMouse / ResolveOmniMouse / ResolveOmniKeyboard,
//! Mouse/AsusMouse.cs, Keyboard/AsusKeyboard.cs and the *Omni models); implemented here from those
//! facts. Three 64-byte vendor collections on interface 2 of the receiver:
//!
//!   mi_02&col01  report 01: pair list
//!       01 A0 00 00  ->  from [5] on, 4-byte slots, [0..2] = product id of a paired device
//!                        (little endian), a zero id ends the list
//!   mi_02&col02  report 02: the keyboard (AsusKeyboard)
//!       02 12 01     ->  02 12 01 .. .. .. [6] battery % .. .. [9] 1 = charging
//!                        (Falchion RX LP: [6] is a 0..10 gauge, the % is in [11])
//!   mi_02&col03  report 03: the mouse (AsusMouse)
//!       03 12 07     ->  03 12 07 .. .. [5] battery % .. .. .. .. [10] != 0 = charging
//!
//! A device that is off or asleep answers FF AA in bytes 1..2, or (mouse) a level of 0. Only these
//! three reads are sent; nothing that changes a setting.
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};
use crate::util::Deadline;

const VID: u16 = 0x0B05;
const PID: u16 = 0x1ACE;
const WAIT_MS: u64 = 1000;

const RID_PAIRS: u8 = 0x01;
const RID_KEYBOARD: u8 = 0x02;
const RID_MOUSE: u8 = 0x03;

/// Mice as they appear in the receiver's pair list (G-Helper MouseFromOmniPid).
fn mouse_name(pid: u16) -> Option<&'static str> {
    Some(match pid {
        0x1A94 => "ROG Harpe Ace Aim Lab Edition",
        0x1B65 => "ROG Harpe Ace Mini",
        0x1C0E => "ROG Keris II Origin",
        0x1D4E => "ROG Keris II Origin KJP",
        0x1AD7 => "ROG Strix Impact III Wireless",
        0x1A72 => "ROG Gladius III Aimpoint",
        0x1A68 | 0x1A6A => "ROG Keris Wireless Aimpoint",
        0x1B1A | 0x1B18 => "ROG Keris II Ace",
        0x1B68 | 0x1B69 => "ROG Harpe Ace Extreme",
        _ => return None,
    })
}

/// Keyboards as they appear in the receiver's pair list (G-Helper KeyboardFromOmniPid).
fn keyboard_name(pid: u16) -> Option<&'static str> {
    Some(match pid {
        0x1A85 => "ROG Azoth",
        // Not in G-Helper's OMNI table: seen in this receiver's pair list next to a ROG Azoth X
        // (issue #428); it is the Azoth X's own dongle id there (AzothXWireless), read like the Azoth.
        0x1C25 => "ROG Azoth X",
        0x1B42 => "ROG Azoth Extreme",
        0x1CF1 => "ROG Azoth Extreme SE",
        0x1AB0 => "ROG Strix Scope II 96 Wireless",
        0x1B7A => "ROG Strix Scope II 96 RX Wireless",
        0x1B06 => "ROG Falchion RX Low Profile",
        _ => return None,
    })
}

/// Byte holding the keyboard's battery % in the 12 01 answer.
fn keyboard_level_at(pid: u16) -> usize {
    if pid == 0x1B06 { 11 } else { 6 }
}

pub fn pairs_request() -> [u8; 4] {
    [RID_PAIRS, 0xA0, 0x00, 0x00]
}

pub fn keyboard_request() -> [u8; 3] {
    [RID_KEYBOARD, 0x12, 0x01]
}

pub fn mouse_request() -> [u8; 3] {
    [RID_MOUSE, 0x12, 0x07]
}

/// Product ids in the pair list (report id included).
pub fn decode_pairs(r: &[u8]) -> Vec<u16> {
    if r.first() != Some(&RID_PAIRS) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut off = 5;
    while off + 3 < r.len() {
        let pid = u16::from_le_bytes([r[off], r[off + 1]]);
        if pid == 0 {
            break;
        }
        out.push(pid);
        off += 4;
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// Level %, charging.
    Battery(i32, bool),
    Asleep,
}

fn is_error(r: &[u8]) -> bool {
    r.len() >= 3 && r[1] == 0xFF && r[2] == 0xAA
}

/// Keyboard answer to 02 12 01 (report id included); None for an unrelated report.
pub fn decode_keyboard(r: &[u8], level_at: usize) -> Option<Answer> {
    if r.first() != Some(&RID_KEYBOARD) {
        return None;
    }
    if is_error(r) {
        return Some(Answer::Asleep);
    }
    if r.len() <= level_at.max(9) || r[..3] != keyboard_request() {
        return None;
    }
    let level = r[level_at];
    if level > 100 {
        return None;
    }
    Some(Answer::Battery(level as i32, r[9] == 1))
}

/// Mouse answer to 03 12 07 (report id included); None for an unrelated report.
pub fn decode_mouse(r: &[u8]) -> Option<Answer> {
    if r.first() != Some(&RID_MOUSE) {
        return None;
    }
    if is_error(r) {
        return Some(Answer::Asleep);
    }
    if r.len() < 11 || r[..3] != mouse_request() || r[5] > 100 {
        return None;
    }
    // G-Helper: a mouse in standby stops reporting its level (reads 0).
    if r[5] == 0 {
        return Some(Answer::Asleep);
    }
    Some(Answer::Battery(r[5] as i32, r[10] != 0))
}

fn ask<T>(c: &HidColl, req: &[u8], decode: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    let d = HidDev::open(c)?;
    d.ask(req, WAIT_MS, decode)
}

/// The pair list: the first report-01 input after the request, as G-Helper reads it.
fn read_pairs(c: &HidColl) -> Option<Vec<u16>> {
    let d = HidDev::open(c)?;
    d.flush();
    if !d.write(&pairs_request()) {
        return None;
    }
    let end = Deadline::in_ms(WAIT_MS);
    while !end.passed() {
        if let Some(r) = d.read(end.left_ms())
            && r.first() == Some(&RID_PAIRS)
        {
            return Some(decode_pairs(&r));
        }
    }
    None
}

/// "mi_02&col02" etc. in a collection path.
fn is_col(c: &HidColl, col: u8) -> bool {
    c.path.to_lowercase().contains(&format!("mi_02&col{col:02}"))
}

/// The receiver a collection belongs to (G-Helper OmniInstance: the instance part of the path up
/// to its last '&'), so two receivers are read separately.
fn instance(c: &HidColl) -> String {
    let p = c.path.to_lowercase();
    let parts: Vec<&str> = p.split('#').collect();
    if parts.len() < 3 {
        return p;
    }
    match parts[2].rfind('&') {
        Some(cut) if cut > 0 => parts[2][..cut].to_string(),
        _ => parts[2].to_string(),
    }
}

fn status(base: DeviceStatus, a: Option<Answer>) -> DeviceStatus {
    match a {
        Some(Answer::Battery(level, charging)) => DeviceStatus { percent: level, charging, ..base },
        Some(Answer::Asleep) => DeviceStatus { asleep: true, ..base },
        None => DeviceStatus { connected: false, ..base },
    }
}

pub fn poll(ctx: &mut PollContext) {
    if ctx.is_claimed(VID, PID) {
        return;
    }
    let all = ctx.find(VID, PID, |c| c.iface == 2 && c.in_len() >= 8 && c.out_len() >= 8);
    let mut found = Vec::new();
    for pairs in all.iter().filter(|c| is_col(c, 1)) {
        let inst = instance(pairs);
        let sibling = |col: u8| all.iter().find(|c| is_col(c, col) && instance(c) == inst);
        let Some(pids) = read_pairs(pairs) else { continue };
        if let (Some(&pid), Some(c)) = (pids.iter().find(|&&p| mouse_name(p).is_some()), sibling(3)) {
            let base = DeviceStatus {
                key: format!("rog-omni:{pid:04x}"),
                name: mouse_name(pid).unwrap_or_default().into(),
                kind: DeviceKind::Mouse,
                via: "2.4 GHz receiver".into(),
                hw: Some((VID, PID)),
                vid: VID,
                grace_sec: 300,
                ..Default::default()
            };
            let a = ask(c, &mouse_request(), decode_mouse);
            crate::util::debug_log(|| format!("ROG OMNI receiver: mouse {pid:04X} answered {a:?}"));
            found.push(status(base, a));
        }
        if let (Some(&pid), Some(c)) = (pids.iter().find(|&&p| keyboard_name(p).is_some()), sibling(2)) {
            let base = DeviceStatus {
                key: format!("rog-omni:{pid:04x}"),
                name: keyboard_name(pid).unwrap_or_default().into(),
                kind: DeviceKind::Keyboard,
                via: "2.4 GHz receiver".into(),
                hw: Some((VID, PID)),
                vid: VID,
                grace_sec: 300,
                ..Default::default()
            };
            let at = keyboard_level_at(pid);
            let a = ask(c, &keyboard_request(), |r| decode_keyboard(r, at));
            crate::util::debug_log(|| format!("ROG OMNI receiver: keyboard {pid:04X} answered {a:?}"));
            found.push(status(base, a));
        }
        crate::util::debug_log(|| {
            let ids: Vec<String> =
                pids.iter().map(|p| format!("{p:04X}{}", if mouse_name(*p).or(keyboard_name(*p)).is_some() { "" } else { " (unknown, not read)" })).collect();
            format!("ROG OMNI receiver: paired {}", ids.join(", "))
        });
    }
    if found.is_empty() {
        return;
    }
    ctx.claim(VID, PID);
    ctx.out.extend(found);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(head: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[..head.len()].copy_from_slice(head);
        f
    }

    #[test]
    fn requests() {
        assert_eq!(pairs_request(), [0x01, 0xA0, 0x00, 0x00]);
        assert_eq!(keyboard_request(), [0x02, 0x12, 0x01]);
        assert_eq!(mouse_request(), [0x03, 0x12, 0x07]);
    }

    #[test]
    fn pair_list() {
        // Harpe Ace Aim Lab (1A94) and Azoth (1A85) paired.
        let r = frame(&[0x01, 0xA0, 0x00, 0x00, 0x00, 0x94, 0x1A, 0x00, 0x00, 0x85, 0x1A, 0x00, 0x00]);
        assert_eq!(decode_pairs(&r), vec![0x1A94, 0x1A85]);
        assert_eq!(decode_pairs(&frame(&[0x01, 0xA0])), Vec::<u16>::new());
        assert_eq!(decode_pairs(&frame(&[0x02, 0xA0, 0, 0, 0, 0x94, 0x1A])), Vec::<u16>::new());
        assert_eq!(mouse_name(0x1A94), Some("ROG Harpe Ace Aim Lab Edition"));
        assert_eq!(keyboard_name(0x1A85), Some("ROG Azoth"));
        // Recorded on the issue #428 receiver: Azoth X and Harpe Ace Aim Lab.
        let r = frame(&[0x01, 0xA0, 0x00, 0x00, 0x00, 0x25, 0x1C, 0x00, 0x00, 0x94, 0x1A, 0x00, 0x00]);
        assert_eq!(decode_pairs(&r), vec![0x1C25, 0x1A94]);
        assert_eq!(keyboard_name(0x1C25), Some("ROG Azoth X"));
    }

    #[test]
    fn keyboard() {
        let mut r = frame(&[0x02, 0x12, 0x01]);
        r[6] = 77;
        r[9] = 1;
        assert_eq!(decode_keyboard(&r, 6), Some(Answer::Battery(77, true)));
        r[9] = 0;
        assert_eq!(decode_keyboard(&r, 6), Some(Answer::Battery(77, false)));
        assert_eq!(decode_keyboard(&frame(&[0x02, 0xFF, 0xAA]), 6), Some(Answer::Asleep));
        assert_eq!(decode_keyboard(&frame(&[0x02, 0x12, 0x00]), 6), None); // another answer
        assert_eq!(decode_keyboard(&frame(&[0x03, 0x12, 0x01]), 6), None); // the mouse channel
        // Falchion RX LP: gauge at [6], % at [11].
        let mut f = frame(&[0x02, 0x12, 0x01]);
        f[6] = 8;
        f[11] = 81;
        assert_eq!(decode_keyboard(&f, keyboard_level_at(0x1B06)), Some(Answer::Battery(81, false)));
    }

    #[test]
    fn mouse() {
        let mut r = frame(&[0x03, 0x12, 0x07]);
        r[5] = 64;
        r[10] = 1;
        assert_eq!(decode_mouse(&r), Some(Answer::Battery(64, true)));
        r[5] = 0;
        r[10] = 0;
        assert_eq!(decode_mouse(&r), Some(Answer::Asleep));
        assert_eq!(decode_mouse(&frame(&[0x03, 0xFF, 0xAA])), Some(Answer::Asleep));
        assert_eq!(decode_mouse(&frame(&[0x03, 0x12, 0x06])), None);
    }

    #[test]
    fn receivers_group_by_instance() {
        let a = HidColl { path: r"\\?\hid#vid_0b05&pid_1ace&mi_02&col01#8&1a2b3c4d&0&0000#{guid}".into(), ..Default::default() };
        let b = HidColl { path: r"\\?\hid#vid_0b05&pid_1ace&mi_02&col03#8&1a2b3c4d&0&0002#{guid}".into(), ..Default::default() };
        let other = HidColl { path: r"\\?\hid#vid_0b05&pid_1ace&mi_02&col01#8&99999999&0&0000#{guid}".into(), ..Default::default() };
        assert_eq!(instance(&a), instance(&b));
        assert_ne!(instance(&a), instance(&other));
        assert!(is_col(&a, 1) && is_col(&b, 3) && !is_col(&b, 2));
    }
}
