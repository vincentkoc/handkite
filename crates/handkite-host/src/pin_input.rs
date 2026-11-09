use handkite_core::passcode::Passcode;
use std::io::Read;
use zeroize::Zeroizing;

pub fn read_once(mut input: impl Read) -> Result<Passcode, &'static str> {
    let mut bytes = Zeroizing::new(Vec::new());
    input
        .by_ref()
        .take(18)
        .read_to_end(&mut bytes)
        .map_err(|_| "private PIN input failed")?;
    if bytes.len() > 17 {
        return Err("invalid PIN input length");
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| "PIN input is not UTF-8")?
        .to_owned();
    Passcode::new(value).map_err(|_| "PIN must contain 4 to 16 ASCII digits")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_one_numeric_line_only() {
        assert_eq!(read_once(&b"123456\n"[..]).unwrap().len(), 6);
        assert_eq!(read_once(&b"123456\r\n"[..]).unwrap().len(), 6);
        assert!(read_once(&b"123456\n7"[..]).is_err());
        assert!(read_once(&b"12345678901234567\n"[..]).is_err());
        assert!(read_once(&b"12x456\n"[..]).is_err());
    }
}
