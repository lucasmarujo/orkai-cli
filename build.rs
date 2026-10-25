use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

const ICON: &str = "assets/orkai.ico";
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const MAIN_ICON: u16 = 1;
const MAIN_VERSION: u16 = 1;
const ICON_FLAGS: u16 = 0x1010;
const GROUP_FLAGS: u16 = 0x1030;
const VERSION_FLAGS: u16 = 0x0030;
const ICO_HEADER: usize = 6;
const ICO_ENTRY: usize = 16;
const ENTRY_SHAPE: usize = 8;
const RESOURCE_HEADER: u32 = 32;
const FIXED_SIGNATURE: u32 = 0xFEEF_04BD;
const FIXED_STRUCT_VERSION: u32 = 0x0001_0000;
const FIXED_FLAGS_MASK: u32 = 0x3F;
const VOS_NT_WINDOWS32: u32 = 0x0004_0004;
const VFT_APP: u32 = 1;
const STRING_TABLE: &str = "040904B0";
const TRANSLATION: u32 = 0x04B0_0409;
const BINARY_NODE: u16 = 0;
const TEXT_NODE: u16 = 1;

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    if !windows {
        return Ok(());
    }
    let msvc = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|target_env| target_env == "msvc");
    let out = env::var_os("OUT_DIR").map(PathBuf::from).ok_or_else(|| io::Error::other("OUT_DIR is not set"))?;
    let res = out.join("orkai.res");
    fs::write(&res, resource_file(&fs::read(ICON)?)?)?;
    if msvc {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        let obj = out.join("orkai_res.o");
        if std::process::Command::new("windres")
            .args(["-J", "res", "-O", "coff"])
            .arg(&res)
            .arg(&obj)
            .status()
            .is_ok_and(|s| s.success())
        {
            println!("cargo:rustc-link-arg-bins={}", obj.display());
        }
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{ICON} is not a valid icon"))
}

fn read_u16(data: &[u8], at: usize) -> io::Result<u16> {
    data.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or_else(invalid)
}

fn read_u32(data: &[u8], at: usize) -> io::Result<usize> {
    let bytes = data.get(at..at + 4).ok_or_else(invalid)?;
    usize::try_from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).map_err(|_| invalid())
}

fn resource_file(icon: &[u8]) -> io::Result<Vec<u8>> {
    if read_u16(icon, 0)? != 0 || read_u16(icon, 2)? != 1 {
        return Err(invalid());
    }
    let count = read_u16(icon, 4)?;
    let mut out = Vec::new();
    push_resource(&mut out, 0, 0, 0, &[])?;
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&count.to_le_bytes());
    for index in 0..count {
        let entry = ICO_HEADER + usize::from(index) * ICO_ENTRY;
        let size = read_u32(icon, entry + 8)?;
        let offset = read_u32(icon, entry + 12)?;
        let image = icon.get(offset..offset + size).ok_or_else(invalid)?;
        let id = index + 1;
        push_resource(&mut out, RT_ICON, id, ICON_FLAGS, image)?;
        group.extend_from_slice(icon.get(entry..entry + ENTRY_SHAPE).ok_or_else(invalid)?);
        group.extend_from_slice(&u32::try_from(size).map_err(|_| invalid())?.to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_resource(&mut out, RT_GROUP_ICON, MAIN_ICON, GROUP_FLAGS, &group)?;
    push_resource(&mut out, RT_VERSION, MAIN_VERSION, VERSION_FLAGS, &version_info()?)?;
    Ok(out)
}

fn package(key: &str) -> io::Result<String> {
    env::var(key).map_err(|_| io::Error::other(format!("{key} is not set")))
}

fn version_part(key: &str) -> io::Result<u32> {
    package(key)?.parse::<u16>().map(u32::from).map_err(|_| io::Error::other(format!("{key} is not a 16-bit number")))
}

fn version_info() -> io::Result<Vec<u8>> {
    let name = package("CARGO_PKG_NAME")?;
    let version = package("CARGO_PKG_VERSION")?;
    let high = (version_part("CARGO_PKG_VERSION_MAJOR")? << 16) | version_part("CARGO_PKG_VERSION_MINOR")?;
    let low = version_part("CARGO_PKG_VERSION_PATCH")? << 16;
    let mut fixed = Vec::new();
    for value in [
        FIXED_SIGNATURE,
        FIXED_STRUCT_VERSION,
        high,
        low,
        high,
        low,
        FIXED_FLAGS_MASK,
        0,
        VOS_NT_WINDOWS32,
        VFT_APP,
        0,
        0,
        0,
    ] {
        fixed.extend_from_slice(&value.to_le_bytes());
    }
    let strings = [
        ("FileDescription", package("CARGO_PKG_DESCRIPTION")?),
        ("FileVersion", version.clone()),
        ("InternalName", name.clone()),
        ("OriginalFilename", format!("{name}.exe")),
        ("ProductName", name),
        ("ProductVersion", version),
    ];
    let strings = strings
        .iter()
        .map(|(key, text)| version_node(key, TEXT_NODE, &utf16(text), &[]))
        .collect::<io::Result<Vec<_>>>()?;
    let table = version_node(STRING_TABLE, TEXT_NODE, &[], &strings)?;
    let translation = version_node("Translation", BINARY_NODE, &TRANSLATION.to_le_bytes(), &[])?;
    let children = [
        version_node("StringFileInfo", TEXT_NODE, &[], &[table])?,
        version_node("VarFileInfo", TEXT_NODE, &[], &[translation])?,
    ];
    version_node("VS_VERSION_INFO", BINARY_NODE, &fixed, &children)
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn version_node(key: &str, kind: u16, value: &[u8], children: &[Vec<u8>]) -> io::Result<Vec<u8>> {
    let too_large = || io::Error::other(format!("version resource {key} is too large"));
    let value_length = if kind == TEXT_NODE { value.len() / 2 } else { value.len() };
    let mut node = vec![0; 6];
    node.extend_from_slice(&utf16(key));
    node.resize(node.len().next_multiple_of(4), 0);
    node.extend_from_slice(value);
    for child in children {
        node.resize(node.len().next_multiple_of(4), 0);
        node.extend_from_slice(child);
    }
    let length = u16::try_from(node.len()).map_err(|_| too_large())?;
    let value_length = u16::try_from(value_length).map_err(|_| too_large())?;
    node[0..2].copy_from_slice(&length.to_le_bytes());
    node[2..4].copy_from_slice(&value_length.to_le_bytes());
    node[4..6].copy_from_slice(&kind.to_le_bytes());
    Ok(node)
}

fn push_resource(out: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) -> io::Result<()> {
    out.extend_from_slice(&u32::try_from(data.len()).map_err(|_| invalid())?.to_le_bytes());
    out.extend_from_slice(&RESOURCE_HEADER.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(4), 0);
    Ok(())
}
