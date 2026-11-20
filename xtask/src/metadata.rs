// Cargo metadata --no-deps lists only workspace packages. Read just the
// packages array and its name fields, ignoring the rest of the JSON document.
pub fn package_names(json: &str) -> Option<Vec<&str>> {
    let mut array = field(json, "packages")?
        .strip_prefix('[')?
        .strip_suffix(']')?
        .trim();
    let mut names = Vec::new();
    while !array.is_empty() {
        let (package, rest) = take_value(array)?;
        let name = field(package, "name")?
            .strip_prefix('"')?
            .strip_suffix('"')?;
        names.push(name);
        let rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        array = rest.strip_prefix(',')?.trim_start();
        if array.is_empty() {
            return None;
        }
    }
    (!names.is_empty()).then_some(names)
}

fn field<'a>(object: &'a str, field_name: &str) -> Option<&'a str> {
    let mut fields = object.trim_start().strip_prefix('{')?;
    loop {
        let (key, rest) = take_string(fields.trim_start())?;
        let rest = rest.trim_start().strip_prefix(':')?;
        let (value, rest) = take_value(rest.trim_start())?;
        if key.strip_prefix('"')?.strip_suffix('"')? == field_name {
            return Some(value);
        }
        fields = rest.trim_start().strip_prefix(',')?;
    }
}

fn take_string(input: &str) -> Option<(&str, &str)> {
    let mut bytes = input.strip_prefix('"')?.bytes().enumerate();
    while let Some((index, byte)) = bytes.next() {
        match byte {
            b'"' => return Some(input.split_at(index + 2)),
            b'\\' => {
                bytes.next()?;
            }
            _ => {}
        }
    }
    None
}

fn take_value(input: &str) -> Option<(&str, &str)> {
    match input.as_bytes().first()? {
        b'"' => take_string(input),
        b'{' | b'[' => {
            let mut rest = input;
            let mut depth = 0;
            while let Some(first) = rest.as_bytes().first() {
                match first {
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(input.split_at(input.len() - rest.len() + 1));
                        }
                    }
                    b'"' => {
                        rest = take_string(rest)?.1;
                        continue;
                    }
                    _ => {}
                }
                rest = &rest[rest.chars().next()?.len_utf8()..];
            }
            None
        }
        _ => {
            let end = input.find([',', '}', ']']).unwrap_or(input.len());
            (end != 0).then(|| input.split_at(end))
        }
    }
}
