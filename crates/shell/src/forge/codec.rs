//! Small pure encoders the OAuth flows need: percent-encoding, base64url,
//! SHA-256, the `Link` header and PKCE. Written here because none of them is
//! worth a crate outside the PRD's build-vs-library table, and each is
//! checked against published test vectors.

/// Percent-encodes everything except the RFC 3986 unreserved characters, so
/// the result is safe in a query string and in a form body.
pub fn pct_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Reads `%XX` escapes (and `+` as a space, as query strings write it).
/// `None` for a broken escape or bytes that are not UTF-8.
pub fn pct_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' => {
                let hex = text.get(at + 1..at + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                at += 3;
            }
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            other => {
                out.push(other);
                at += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `a=1&b=x%20y`, ready for a form body or a query string.
pub fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", pct_encode(key), pct_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Splits a query string into decoded pairs. Pairs that do not decode are
/// dropped.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((pct_decode(key)?, pct_decode(value)?))
        })
        .collect()
}

const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url without padding (RFC 4648 section 5), as PKCE writes it.
pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            out.push(URL_SAFE[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_be_bytes();
    }
    out
}

/// A PKCE pair (RFC 7636, method S256).
#[derive(Clone)]
pub struct Pkce {
    verifier: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pkce(..)")
    }
}

impl Pkce {
    /// The verifier is the base64url of the random bytes: 32 bytes give the
    /// 43 characters RFC 7636 asks for at least.
    pub fn from_random(random: &[u8; 32]) -> Pkce {
        Pkce {
            verifier: base64url(random),
        }
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn challenge(&self) -> String {
        base64url(&sha256(self.verifier.as_bytes()))
    }
}

/// The address of the next page from a `Link` header, if there is one:
/// `<https://api.example/x?page=2>; rel="next", <...>; rel="last"`.
pub fn next_link(header: &str) -> Option<String> {
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let close = after.find('>')?;
        let url = after[..close].trim();
        let tail = &after[close + 1..];
        // The parameters run to the next `<` (the next link).
        let params = &tail[..tail.find('<').unwrap_or(tail.len())];
        let is_next = params.split(';').any(|param| {
            let param = param.trim().trim_end_matches(',').trim();
            match param.split_once('=') {
                Some((name, value)) if name.trim().eq_ignore_ascii_case("rel") => value
                    .trim()
                    .trim_matches('"')
                    .split_whitespace()
                    .any(|rel| rel.eq_ignore_ascii_case("next")),
                _ => false,
            }
        });
        if is_next {
            return Some(url.to_owned());
        }
        rest = tail;
    }
    None
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        for (input, digest) in [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            assert_eq!(hex(&sha256(input.as_bytes())), digest, "{input:?}");
        }
        // Padding boundaries: 55, 56 and 64 bytes.
        assert_eq!(
            hex(&sha256(&[b'a'; 1_000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
        assert_eq!(sha256(&[0; 55]).len(), 32);
        assert_ne!(sha256(&[0; 55]), sha256(&[0; 56]));
        assert_ne!(sha256(&[0; 56]), sha256(&[0; 64]));
    }

    #[test]
    fn pkce_matches_rfc_7636_appendix_b() {
        // The RFC's verifier is given as text, so build the pair directly.
        let pkce = Pkce {
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
        };
        assert_eq!(
            pkce.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let made = Pkce::from_random(&[7; 32]);
        assert_eq!(made.verifier().len(), 43);
        assert!(!format!("{made:?}").contains(made.verifier()));
    }

    #[test]
    fn base64url_matches_rfc_4648_without_padding() {
        for (input, text) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(input.as_bytes()), text);
        }
        assert_eq!(base64url(&[0xfb, 0xff, 0xfe]), "-__-");
    }

    #[test]
    fn percent_coding_round_trips_and_guards_delimiters() {
        assert_eq!(pct_encode("a b&c=d/é~"), "a%20b%26c%3Dd%2F%C3%A9~");
        assert_eq!(pct_decode("a%20b+c%C3%A9").as_deref(), Some("a b cé"));
        assert_eq!(pct_decode("%zz"), None);
        assert_eq!(pct_decode("%4"), None);
        assert_eq!(pct_decode("%FF"), None);
        assert_eq!(
            form_body(&[("client_id", "x y"), ("scope", "repo&evil=1")]),
            "client_id=x%20y&scope=repo%26evil%3D1"
        );
        assert_eq!(
            parse_query("code=a%2Fb&state=s&flag&x="),
            vec![
                ("code".into(), "a/b".into()),
                ("state".into(), "s".into()),
                ("flag".into(), String::new()),
                ("x".into(), String::new()),
            ]
        );
    }

    #[test]
    fn a_padded_link_is_trimmed_to_the_address_it_names() {
        assert_eq!(
            next_link("< https://x/y?p=2 >; rel=next").as_deref(),
            Some("https://x/y?p=2")
        );
        assert_eq!(
            next_link("<\thttp://127.0.0.1:9/n\n>; rel=\"next\"").as_deref(),
            Some("http://127.0.0.1:9/n")
        );
    }

    #[test]
    fn the_next_link_is_found_among_others() {
        let header = r#"<https://api.github.com/user/repos?page=1>; rel="prev", <https://api.github.com/user/repos?page=3&per_page=100>; rel="next", <https://api.github.com/user/repos?page=9>; rel="last""#;
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://api.github.com/user/repos?page=3&per_page=100")
        );
        assert_eq!(
            next_link("<https://x/a,b>; rel=next").as_deref(),
            Some("https://x/a,b")
        );
        assert_eq!(next_link(r#"<https://x/1>; rel="prev""#), None);
        assert_eq!(next_link(r#"<https://x/1>; rel="nextish""#), None);
        assert_eq!(next_link(""), None);
        assert_eq!(next_link("<unterminated; rel=next"), None);
    }

    proptest! {
        #[test]
        fn encoded_text_decodes_to_itself(text in "\\PC{0,40}") {
            prop_assert_eq!(pct_decode(&pct_encode(&text)), Some(text));
        }

        #[test]
        fn an_encoded_form_never_hides_a_delimiter(key in "\\PC{0,10}", value in "\\PC{0,10}") {
            let body = form_body(&[(&key, &value)]);
            prop_assert_eq!(body.matches('&').count(), 0);
            prop_assert_eq!(body.matches('=').count(), 1);
            prop_assert_eq!(parse_query(&body), vec![(key, value)]);
        }

        #[test]
        fn base64url_uses_only_url_safe_characters(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let text = base64url(&bytes);
            prop_assert!(text.bytes().all(|b| URL_SAFE.contains(&b)));
            prop_assert_eq!(text.len(), (bytes.len() * 4).div_ceil(3));
        }

        #[test]
        fn the_link_reader_never_panics(header in "\\PC{0,80}") {
            let _ = next_link(&header);
        }
    }
}
