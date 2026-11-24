//! Cryptographically secure random bytes from the operating system, for the
//! PKCE verifier and the OAuth `state`. Not `RandomState` or the clock:
//! both are guessable.

/// Fills `buffer` from the OS random source. Fails rather than falling back
/// to something weaker.
#[cfg(windows)]
pub fn fill(buffer: &mut [u8]) -> Result<(), String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    // SAFETY: the pointer and length describe `buffer`, which is valid and
    // writable; a null algorithm handle with the system-preferred flag is
    // the documented way to ask for the default RNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err("the system random source failed".into())
    }
}

#[cfg(not(windows))]
pub fn fill(buffer: &mut [u8]) -> Result<(), String> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buffer))
        .map_err(|_| "the system random source failed".to_owned())
}

/// `N` random bytes.
pub fn bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut out = [0u8; N];
    fill(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_draws_differ_and_are_not_zero() {
        let a = bytes::<32>().unwrap();
        let b = bytes::<32>().unwrap();
        assert_ne!(a, b);
        assert_ne!(a, [0; 32]);
    }
}
