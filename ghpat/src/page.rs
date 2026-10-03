//! SensitivePage：daemon 内唯一敏感状态（规格 §5.1、§8.2）
//!
//! 单页 4KiB mmap(MAP_PRIVATE|MAP_ANONYMOUS)，整页 mlock + MADV_DONTDUMP，
//! 进程生命周期内不 munlock、不 realloc。布局：
//!   offset 0..32   : identity（age x25519 私钥原始 32 字节）
//!   offset 32..40  : pat_len (u64 LE)
//!   offset 40..296 : pat_buf ([u8; 256])
//!   offset 296..   : 预留（保持页内零填充）

use std::io;

const PAGE_SIZE: usize = 4096;
const OFF_IDENTITY: usize = 0;
const OFF_PAT_LEN: usize = 32;
const OFF_PAT_BUF: usize = 40;
pub const PAT_CAP: usize = 256;

pub struct SensitivePage {
    page: *mut u8,
}

// 页由本结构独占管理，跨线程通过 Arc<Mutex<..>> 访问（§6.4）
unsafe impl Send for SensitivePage {}
unsafe impl Sync for SensitivePage {}

impl SensitivePage {
    pub fn new() -> io::Result<Self> {
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                PAGE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            if libc::mlock(ptr, PAGE_SIZE) != 0 {
                let e = io::Error::last_os_error();
                libc::munmap(ptr, PAGE_SIZE);
                return Err(e);
            }
            if libc::madvise(ptr, PAGE_SIZE, libc::MADV_DONTDUMP) != 0 {
                let e = io::Error::last_os_error();
                libc::munmap(ptr, PAGE_SIZE);
                return Err(e);
            }
            Ok(SensitivePage { page: ptr as *mut u8 })
        }
    }

    /// volatile 逐字节写入（防止编译器优化掉敏感数据写操作）
    unsafe fn write_volatile(&self, off: usize, src: &[u8]) {
        for (i, b) in src.iter().enumerate() {
            self.page.add(off + i).write_volatile(*b);
        }
    }

    unsafe fn zero_volatile(&self, off: usize, len: usize) {
        for i in 0..len {
            self.page.add(off + i).write_volatile(0u8);
        }
    }

    /// 私钥写入页内（一次性，启动时）
    pub fn set_identity(&self, raw: &[u8; 32]) {
        unsafe { self.write_volatile(OFF_IDENTITY, raw) }
    }

    /// 从页内读取私钥（用于重建 age Identity 解密；瞬态副本由调用方 zeroize）
    pub fn identity_raw(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        unsafe {
            for i in 0..32 {
                out[i] = self.page.add(OFF_IDENTITY + i).read_volatile();
            }
        }
        out
    }

    /// 原地 zeroize 旧值后覆写（§5.1 set_pat）
    pub fn set_pat(&self, pat: &str) -> Result<(), ()> {
        if pat.len() > PAT_CAP {
            return Err(());
        }
        unsafe {
            let old_len = self.pat_len();
            // 先抹旧 PAT 内容与长度
            self.zero_volatile(OFF_PAT_BUF, old_len.max(pat.len()));
            self.zero_volatile(OFF_PAT_LEN, 8);
            self.write_volatile(OFF_PAT_BUF, pat.as_bytes());
            self.write_volatile(OFF_PAT_LEN, &(pat.len() as u64).to_le_bytes());
        }
        Ok(())
    }

    unsafe fn pat_len(&self) -> usize {
        let mut b = [0u8; 8];
        for i in 0..8 {
            b[i] = self.page.add(OFF_PAT_LEN + i).read_volatile();
        }
        u64::from_le_bytes(b) as usize
    }

    /// 以引用暴露，禁止克隆到页外（§5.1）
    pub fn pat(&self) -> Option<&str> {
        unsafe {
            let len = self.pat_len();
            if len == 0 || len > PAT_CAP {
                return None;
            }
            let slice = std::slice::from_raw_parts(self.page.add(OFF_PAT_BUF), len);
            std::str::from_utf8(slice).ok()
        }
    }

    pub fn is_armed(&self) -> bool {
        unsafe { self.pat_len() > 0 }
    }

    /// volatile 写零整页（§5.4）
    pub fn zeroize_all(&self) {
        unsafe { self.zero_volatile(0, PAGE_SIZE) }
    }
}

impl Drop for SensitivePage {
    fn drop(&mut self) {
        self.zeroize_all();
        unsafe {
            libc::munlock(self.page as *mut libc::c_void, PAGE_SIZE);
            libc::munmap(self.page as *mut libc::c_void, PAGE_SIZE);
        }
    }
}

/// PAT 指纹（§7.2）：识别已知前缀，显示 <prefix>…<末4位>；未知前缀显示 <前8字符>…<末4位>
pub fn fingerprint(pat: &str) -> String {
    let known = ["ghp_", "github_pat_", "gho_", "ghs_", "ghu_"];
    for p in known {
        if let Some(rest) = pat.strip_prefix(p) {
            if rest.len() >= 4 {
                return format!("{p}…{}", &rest[rest.len() - 4..]);
            }
        }
    }
    if pat.len() >= 12 {
        format!("{}…{}", &pat[..8], &pat[pat.len() - 4..])
    } else {
        "…".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_set_get_zeroize() {
        let page = SensitivePage::new().unwrap();
        assert!(!page.is_armed());
        assert!(page.pat().is_none());
        page.set_pat("ghp_abcdefghijklmnopqrstuv0123456789").unwrap();
        assert_eq!(page.pat(), Some("ghp_abcdefghijklmnopqrstuv0123456789"));
        page.set_pat("ghp_short").unwrap();
        assert_eq!(page.pat(), Some("ghp_short"));
        assert!(page.set_pat(&"x".repeat(257)).is_err());
        page.zeroize_all();
        assert!(page.pat().is_none());
        assert_eq!(page.identity_raw(), [0u8; 32]);
    }

    #[test]
    fn fingerprints() {
        assert_eq!(fingerprint("ghp_123456789012345678901234567899xYz"), "ghp_…9xYz");
        assert_eq!(
            fingerprint("github_pat_11ABC0123aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa9xYz"),
            "github_pat_…9xYz"
        );
        assert_eq!(fingerprint("totallyunknownprefix01234567899xYz"), "totallyu…9xYz");
    }
}
