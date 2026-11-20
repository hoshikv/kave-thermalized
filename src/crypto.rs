use aes::Aes128;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;

const KEY: &[u8; 16] = b"thermalopenssl.h";

type Aes128CbcDec = Decryptor<Aes128>;

pub fn decrypt(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(format!("invalid AES-CBC length {}", data.len()));
    }

    let mut buffer = data.to_vec();
    let plain = Aes128CbcDec::new(KEY.into(), KEY.into())
        .decrypt_padded_mut::<NoPadding>(&mut buffer)
        .map_err(|error| format!("AES-CBC decrypt failed: {error}"))?;

    let end = plain
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(plain.len());
    Ok(plain[..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::{block_padding::NoPadding, BlockEncryptMut};
    use cbc::Encryptor;

    type Aes128CbcEnc = Encryptor<Aes128>;

    #[test]
    fn round_trips() {
        let mut plain = b"[A]\ntrig\t1 2 3\n".to_vec();
        while !plain.len().is_multiple_of(16) {
            plain.push(0);
        }
        let mut buffer = plain.clone();
        Aes128CbcEnc::new(KEY.into(), KEY.into())
            .encrypt_padded_mut::<NoPadding>(&mut buffer, plain.len())
            .unwrap();
        let decrypted = decrypt(&buffer).unwrap();
        assert_eq!(decrypted, b"[A]\ntrig\t1 2 3\n");
    }
}
