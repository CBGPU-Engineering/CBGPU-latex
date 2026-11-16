//! Disk-backed ZIP normalization. No archive path is ever extracted on the host.
use crate::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Write},
    path::Path,
};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inspection {
    pub files: Vec<String>,
    pub candidates: Vec<String>,
    pub suggested_main: String,
    pub compiler: String,
    pub warnings: Vec<String>,
    pub size: u64,
    pub count: usize,
}
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 500
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && !Regex::new(r"^[A-Za-z]:").unwrap().is_match(name)
        && !name
            .trim_end_matches('/')
            .split('/')
            .any(|v| v.is_empty() || v == "." || v == "..")
}
pub fn inspect(source: &Path, output: &Path) -> Result<Inspection> {
    let file = File::open(source).map_err(|_| "Cannot read ZIP")?;
    let mut archive = ZipArchive::new(file).map_err(|_| "Invalid ZIP archive")?;
    if archive.len() > 1100 {
        return Err("Project supports at most 1,000 files".into());
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut size = 0u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| "Encrypted, corrupt or unsupported ZIP")?;
        let name = entry
            .name()
            .map_err(|_| "Invalid ZIP filename encoding")?
            .into_owned();
        if !safe_name(&name) || entry.is_symlink() {
            return Err("Unsafe archive path or symbolic link".into());
        }
        if entry.is_dir() {
            continue;
        }
        let parts: Vec<_> = name.split('/').collect();
        if parts
            .iter()
            .any(|v| matches!(*v, "__MACOSX" | ".git" | "node_modules"))
            || matches!(parts.last(), Some(&".DS_Store" | &"Thumbs.db"))
        {
            continue;
        }
        let key: String = name.nfc().case_fold().collect();
        if !seen.insert(key) {
            return Err("Duplicate archive filename".into());
        }
        if entry.size() > 1024 * 1024
            && u128::from(entry.size()) > u128::from(entry.compressed_size().max(1)) * 300
        {
            return Err("Excessive ZIP compression ratio".into());
        }
        size = size.checked_add(entry.size()).ok_or("ZIP size overflow")?;
        if entries.len() >= 1000 {
            return Err("Project supports at most 1,000 files".into());
        }
        entries.push((name.to_owned(), index));
    }
    if entries.is_empty() {
        return Err("ZIP contains no project files".into());
    }
    // Reject impossible disk reservations before scanning any large TeX member.
    ensure_space(
        output.parent().ok_or("Missing output directory")?,
        size.saturating_add(4 * 1024 * 1024),
    )?;
    let mut warnings = Vec::new();
    loop {
        let wrapper = entries[0].0.split_once('/').map(|v| v.0.to_owned());
        let Some(wrapper) = wrapper else { break };
        if !entries
            .iter()
            .all(|(n, _)| n.split_once('/').is_some_and(|v| v.0 == wrapper))
        {
            break;
        }
        for (name, _) in &mut entries {
            *name = name[wrapper.len() + 1..].to_owned();
        }
        warnings.push(format!("Removed shared enclosing folder {wrapper}"));
    }
    let document = Regex::new(r"\\documentclass(?:\s*\[[^\]]*\])?\s*\{([^}]+)\}").unwrap();
    let magic =
        Regex::new(r"(?im)^%\s*!\s*TeX\s+program\s*=\s*(xelatex|lualatex|pdflatex)").unwrap();
    let fonts = Regex::new(
        r"\\(?:usepackage(?:\[[^\]]*\])?\{(?:fontspec|kotex|xeCJK)|setmainfont)|[가-힣]",
    )
    .unwrap();
    let mut tex = Vec::new();
    let mut candidates = Vec::new();
    let mut compiler = "pdflatex".to_owned();
    for (name, index) in &entries {
        if !name.to_ascii_lowercase().ends_with(".tex") {
            continue;
        }
        tex.push(name.clone());
        let member = archive.by_index(*index).map_err(|_| "Corrupt ZIP")?;
        let (is_main, program, needs_xe) = scan_tex(member, &document, &magic, &fonts)?;
        if is_main {
            candidates.push(name.clone());
        }
        if let Some(program) = program {
            compiler = program;
        } else if compiler == "pdflatex" && needs_xe {
            compiler = "xelatex".into();
        }
    }
    if tex.is_empty() {
        return Err("ZIP contains no .tex source".into());
    }
    if candidates.is_empty() {
        candidates = tex;
    }
    candidates.sort_by_key(|p| {
        (
            !matches!(
                p.rsplit('/').next().unwrap().to_ascii_lowercase().as_str(),
                "main.tex" | "manuscript.tex" | "paper.tex"
            ),
            p.matches('/').count(),
            p.clone(),
        )
    });
    ensure_space(
        output.parent().ok_or("Missing output directory")?,
        size.saturating_add(4 * 1024 * 1024),
    )?;
    use std::os::unix::fs::OpenOptionsExt;
    let target = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(output)
        .map_err(|_| "Cannot create normalized ZIP")?;
    let mut writer = ZipWriter::new(target);
    for (name, index) in &entries {
        writer
            .start_file(
                name,
                SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .large_file(true),
            )
            .map_err(|_| "Cannot normalize ZIP")?;
        std::io::copy(
            &mut archive.by_index(*index).map_err(|_| "Corrupt ZIP")?,
            &mut writer,
        )
        .map_err(|_| "ZIP CRC or output failure")?;
    }
    writer
        .finish()
        .map_err(|_| "Cannot finish normalized ZIP")?
        .flush()
        .map_err(|_| "Cannot flush normalized ZIP")?;
    Ok(Inspection {
        files: entries.iter().map(|v| v.0.clone()).collect(),
        suggested_main: candidates[0].clone(),
        candidates,
        compiler,
        warnings,
        size,
        count: entries.len(),
    })
}
pub fn blank(path: &Path) -> Result<()> {
    let mut writer = ZipWriter::new(File::create(path).map_err(|_| "Cannot create project ZIP")?);
    writer
        .start_file("main.tex", SimpleFileOptions::default())
        .map_err(|_| "Cannot initialize project")?;
    writer
        .write_all(b"\\documentclass{article}\n\\begin{document}\n\n\\end{document}\n")
        .map_err(|_| "Cannot write project source")?;
    writer.finish().map_err(|_| "Cannot finish project ZIP")?;
    Ok(())
}

/// No arbitrary project-byte cap; retain configured global free disk headroom.
pub fn ensure_space(path: &Path, bytes: u64) -> Result<()> {
    let reserve = std::env::var("CBGPU_LATEX_DISK_RESERVE_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1024 * 1024 * 1024);
    if fs2::available_space(path)
        .map_err(|_| "Cannot measure project disk space")?
        .saturating_sub(reserve)
        < bytes
    {
        return Err("Insufficient free project storage; existing projects were preserved".into());
    }
    Ok(())
}
fn scan_tex(
    mut input: impl Read,
    document: &Regex,
    magic: &Regex,
    fonts: &Regex,
) -> Result<(bool, Option<String>, bool)> {
    let mut buffer = [0u8; 65536];
    let mut pending = Vec::new();
    let mut raw_tail = String::new();
    let mut clean_tail = String::new();
    let mut comment = false;
    let mut previous = '\0';
    let mut first = true;
    let mut main = false;
    let mut program = None;
    let mut needs_xe = false;
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| "Corrupt ZIP member or CRC")?;
        if count == 0 {
            if !pending.is_empty() {
                return Err("TeX source must be UTF-8".into());
            }
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => return Err("TeX source must be UTF-8".into()),
        };
        let mut text =
            std::str::from_utf8(&pending[..valid]).map_err(|_| "TeX source must be UTF-8")?;
        if first {
            text = text.trim_start_matches('\u{feff}');
            first = false;
        }
        let raw = format!("{raw_tail}{text}");
        if program.is_none() {
            program = magic.captures(&raw).map(|m| m[1].to_ascii_lowercase());
        }
        needs_xe |= fonts.is_match(&raw);
        let mut clean = clean_tail.clone();
        for ch in text.chars() {
            if ch == '\n' {
                comment = false;
            } else if ch == '%' && previous != '\\' {
                comment = true;
            }
            if !comment {
                clean.push(ch);
            }
            previous = ch;
        }
        main |= document.captures_iter(&clean).any(|c| &c[1] != "subfiles");
        raw_tail = tail(&raw);
        clean_tail = tail(&clean);
        pending.drain(..valid);
    }
    Ok((main, program, needs_xe))
}
fn tail(text: &str) -> String {
    let mut chars = text.chars().rev().take(4096).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}
