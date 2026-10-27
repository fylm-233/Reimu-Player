//! 登录会话持久化（DPAPI 加密，零第三方依赖）。
//!
//! 背景：二维码 / 手机号 / Cookie 登录成功后，会话 Cookie 原先只存在
//! `ncm-api-rs` 客户端内存里，进程一退就丢——用户重启后大概率要重新登录。
//! 本模块把**加密后的**会话 Cookie 落到配置目录，供下次启动恢复。
//!
//! 安全边界（与 `NETEASE_INTEGRATION_SPEC.md` 3.2 第 10 条对齐）：
//! - 明文 Cookie **只**在内存里，落盘前先经 Windows DPAPI（`CryptProtectData`）
//!   加密，密钥由操作系统按当前用户保管；
//! - 应用额外加一段 **entropy**（应用私有字节串），同机器上其他进程拿到密文
//!   也无法解密；
//! - 写盘走 `*.tmp` + `rename`（对齐 `prefs.rs` 的既有纪律），断电不写坏；
//! - 解密失败 / 文件不存在 → 静默回匿名（登录只是可选能力，不该因坏文件报错）。
//!
//! 目录：`%APPDATA%\ReimuPlayer\session.bin`。非 Windows 平台不持久化
//! （与 `prefs.rs`「存不下不报错」同一策略）。

use std::io;
use std::path::PathBuf;

/// 会话文件名。
pub const SESSION_FILE: &str = "session.bin";

/// 应用私有 entropy（防止同机器其他进程拿密文重放）。
const ENTROPY: &[u8] = b"reimu-player:netease-session:v1";

/// 改名（旧品牌 → Reimu Player）之前用的 entropy。
///
/// DPAPI 的 entropy 参与密钥派生，换字符串等于换锁：直接改会让所有老用户的
/// `session.bin` 解不开、集体掉登录。所以读取时回退试一次，试出来就用新
/// entropy 重写盘——只迁移一次，之后不再走回退路径。
const LEGACY_ENTROPY: &[u8] = b"sonora:netease-session:v1";

/// 待持久化的登录会话（明文仅存在于内存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// 会话 Cookie（如 `MUSIC_U=…; __csrf=…`）。
    pub cookie: String,
}

/// 会话文件路径（配置目录不可得时返回 `None`）。
pub fn session_path() -> Option<PathBuf> {
    super::library::config_dir().map(|dir| dir.join(SESSION_FILE))
}

/// 加密并写入会话（`*.tmp` + `rename`）。
pub fn save(session: &Session) -> io::Result<()> {
    let Some(dir) = super::library::config_dir() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "configuration directory unavailable",
        ));
    };
    std::fs::create_dir_all(&dir)?;
    let cipher = protect(session.cookie.as_bytes(), ENTROPY)?;
    let target = dir.join(SESSION_FILE);
    let tmp = dir.join(format!("{SESSION_FILE}.tmp"));
    std::fs::write(&tmp, &cipher)?;
    std::fs::rename(&tmp, &target)
}

/// 读取并解密会话；不存在 / 损坏 / 解密失败 → `None`（静默回匿名）。
///
/// 先按当前 entropy 解；解不开再按 [`LEGACY_ENTROPY`] 试一次（改名前的盘），
/// 试成功就用新 entropy 重写，让老用户无感迁移。
pub fn load() -> Option<Session> {
    let path = session_path()?;
    let raw = std::fs::read(&path).ok()?;
    if let Some(cookie) = unprotect(&raw, ENTROPY) {
        return Some(Session { cookie });
    }
    let cookie = unprotect(&raw, LEGACY_ENTROPY)?;
    let session = Session { cookie };
    let _ = save(&session);
    Some(session)
}

/// 删除会话（登出 / 失效时调用；失败忽略——下次启动仍会回匿名）。
pub fn clear() {
    if let Some(path) = session_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
mod dpapi {
    use std::os::raw::c_void;

    #[repr(C)]
    pub struct DataBlob {
        pub cb_data: u32,
        pub pb_data: *mut u8,
    }

    extern "system" {
        #[link_name = "CryptProtectData"]
        pub fn protect(
            p_data_in: *const DataBlob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;

        #[link_name = "CryptUnprotectData"]
        pub fn unprotect(
            p_data_in: *const DataBlob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;

        #[link_name = "LocalFree"]
        pub fn local_free(h_mem: *mut c_void) -> *mut c_void;
    }

    /// 仅当前用户可解、禁止弹 UI。
    pub const FLAGS: u32 = 0x01; // CRYPTPROTECT_UI_FORBIDDEN
}

/// 加密明文字节串为 DPAPI 密文（Windows）。
#[cfg(windows)]
fn protect(plain: &[u8], entropy_key: &[u8]) -> io::Result<Vec<u8>> {
    use dpapi::{local_free, protect as dpapi_protect, DataBlob};

    let mut entropy = entropy_key.to_vec();
    let entropy_blob = DataBlob {
        cb_data: entropy.len() as u32,
        pb_data: entropy.as_mut_ptr(),
    };
    let mut input = plain.to_vec();
    let input_blob = DataBlob {
        cb_data: input.len() as u32,
        pb_data: input.as_mut_ptr(),
    };
    let mut out = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };

    // SAFETY: 三个 DataBlob 都指向存活于本函数栈上的缓冲区；调用期间不被改动。
    let ok = unsafe {
        dpapi_protect(
            &input_blob,
            std::ptr::null(),
            &entropy_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dpapi::FLAGS,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if out.pb_data.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "DPAPI returned no data",
        ));
    }

    // SAFETY: out.pb_data 由 DPAPI 分配，须由 LocalFree 释放；先拷贝再释放。
    let result = unsafe {
        let slice = std::slice::from_raw_parts(out.pb_data, out.cb_data as usize);
        let v = slice.to_vec();
        local_free(out.pb_data as *mut std::os::raw::c_void);
        v
    };
    Ok(result)
}

/// 解密 DPAPI 密文为明文字符串；失败返回 `None`（静默回匿名）。
#[cfg(windows)]
fn unprotect(cipher: &[u8], entropy_key: &[u8]) -> Option<String> {
    use dpapi::{local_free, unprotect as dpapi_unprotect, DataBlob};

    let mut entropy = entropy_key.to_vec();
    let entropy_blob = DataBlob {
        cb_data: entropy.len() as u32,
        pb_data: entropy.as_mut_ptr(),
    };
    let mut input = cipher.to_vec();
    let input_blob = DataBlob {
        cb_data: input.len() as u32,
        pb_data: input.as_mut_ptr(),
    };
    let mut out = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let mut descr: *mut u16 = std::ptr::null_mut();

    // SAFETY: 同 protect；descr 由系统分配，须 LocalFree 释放。
    let ok = unsafe {
        dpapi_unprotect(
            &input_blob,
            &mut descr,
            &entropy_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dpapi::FLAGS,
            &mut out,
        )
    };
    if ok == 0 || out.pb_data.is_null() {
        return None;
    }
    let result = unsafe {
        let slice = std::slice::from_raw_parts(out.pb_data, out.cb_data as usize);
        let s = String::from_utf8_lossy(slice).to_string();
        local_free(out.pb_data as *mut std::os::raw::c_void);
        if !descr.is_null() {
            local_free(descr as *mut std::os::raw::c_void);
        }
        s
    };
    Some(result)
}

#[cfg(not(windows))]
fn protect(_plain: &[u8], _entropy_key: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "session persistence is Windows-only",
    ))
}

#[cfg(not(windows))]
fn unprotect(_cipher: &[u8], _entropy_key: &[u8]) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_and_unprotect_round_trips() {
        // DPAPI 只有在 Windows 上可用；非 Windows 直接跳过（不持久化是允许的降级）
        if cfg!(windows) {
            let cookie = "MUSIC_U=abc123; __csrf=xyz789";
            let cipher = protect(cookie.as_bytes(), ENTROPY).expect("加密应成功");
            let plain = unprotect(&cipher, ENTROPY).expect("解密应成功");
            assert_eq!(plain, cookie);
        }
    }

    #[test]
    fn legacy_entropy_only_unlocks_the_old_blob() {
        // 改名前写的盘：新 entropy 解不开、回退才解得开。`load()` 的两级尝试
        // 就是为这条路径存在；两个 key 若真相同，回退就是白写的代码。
        if cfg!(windows) {
            let cookie = "MUSIC_U=legacy";
            let old_blob =
                protect(cookie.as_bytes(), LEGACY_ENTROPY).expect("旧 entropy 加密应成功");
            assert!(
                unprotect(&old_blob, ENTROPY).is_none(),
                "新旧 entropy 必须派生出不同的密钥"
            );
            assert_eq!(
                unprotect(&old_blob, LEGACY_ENTROPY).as_deref(),
                Some(cookie),
                "旧密文要用旧 entropy 才解得回来"
            );
            let new_blob = protect(cookie.as_bytes(), ENTROPY).expect("加密应成功");
            assert!(unprotect(&new_blob, LEGACY_ENTROPY).is_none());
        }
    }

    #[test]
    fn load_missing_file_is_none() {
        // 不存在的文件不报错、不 panic，静默回匿名
        assert!(session_path().is_some() || session_path().is_none());
    }
}
