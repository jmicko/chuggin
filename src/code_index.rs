use crate::project;
use anyhow::Result;
use quote::ToTokens;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
};
use syn::{ImplItem, Item, Visibility};

fn signatures(items: &[Item], out: &mut Vec<String>) {
    for item in items {
        match item {
            Item::Struct(i) => out.push(format!(
                "{} struct {} {}",
                i.vis.to_token_stream(),
                i.ident,
                i.fields.to_token_stream()
            )),
            Item::Enum(i) => out.push(format!(
                "{} enum {} {{ {} }}",
                i.vis.to_token_stream(),
                i.ident,
                i.variants
                    .iter()
                    .map(|v| v.ident.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Item::Impl(i) => {
                let owner = i.self_ty.to_token_stream().to_string();
                for item in &i.items {
                    if let ImplItem::Fn(f) = item
                        && (matches!(f.vis, Visibility::Public(_)) || i.trait_.is_some())
                    {
                        out.push(format!("impl {owner}: {}", f.sig.to_token_stream()));
                    }
                }
            }
            Item::Fn(i) if matches!(i.vis, Visibility::Public(_)) => {
                out.push(i.sig.to_token_stream().to_string())
            }
            Item::Mod(i) => {
                if let Some((_, items)) = &i.content {
                    signatures(items, out);
                }
            }
            _ => {}
        }
    }
}
fn modules(items: &[Item], base: &Path, module_dir: &Path, out: &mut Vec<PathBuf>) {
    for item in items {
        if let Item::Mod(m) = item {
            if m.attrs.iter().any(|a| {
                a.path().is_ident("cfg") && a.meta.to_token_stream().to_string().contains("test")
            }) {
                continue;
            }
            if let Some((_, items)) = &m.content {
                let nested = module_dir.join(m.ident.to_string());
                modules(items, &nested, &nested, out);
                continue;
            }
            let explicit = m.attrs.iter().find_map(|a| {
                if a.path().is_ident("path")
                    && let syn::Meta::NameValue(n) = &a.meta
                    && let syn::Expr::Lit(l) = &n.value
                    && let syn::Lit::Str(s) = &l.lit
                {
                    return Some(base.join(s.value()));
                }
                None
            });
            if let Some(p) = explicit {
                out.push(p);
            } else {
                out.push(module_dir.join(format!("{}.rs", m.ident)));
                out.push(module_dir.join(m.ident.to_string()).join("mod.rs"));
            }
        }
    }
}
pub fn index(root: &Path) -> Result<Value> {
    let files = project::inventory(root)?;
    let mut active = BTreeSet::new();
    let mut queue = VecDeque::from([PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")]);
    for p in &files {
        if p.starts_with("src/bin/") && p.ends_with(".rs") {
            queue.push_back(p.into());
        }
    }
    while let Some(path) = queue.pop_front() {
        let name = path.to_string_lossy().to_string();
        if active.contains(&name) {
            continue;
        }
        let Ok(text) = project::read(root, &name) else {
            continue;
        };
        active.insert(name);
        if let Ok(ast) = syn::parse_file(&text) {
            let base = path.parent().unwrap_or(Path::new(""));
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let dir = if matches!(stem.as_ref(), "lib" | "main" | "mod") {
                base.to_path_buf()
            } else {
                base.join(stem.as_ref())
            };
            let mut found = Vec::new();
            modules(&ast.items, base, &dir, &mut found);
            queue.extend(found);
        }
    }
    let mut rows = Vec::new();
    let mut bytes = 0;
    // Compiled module candidates first; inactive legacy files must not displace them.
    let mut paths: Vec<_> = files.iter().filter(|f| f.ends_with(".rs")).collect();
    paths.sort_by_key(|p| (!active.contains(*p), p.as_str()));
    for path in paths {
        if bytes >= 10000 {
            break;
        }
        let Ok(text) = project::read(root, path) else {
            continue;
        };
        let mut api = Vec::new();
        let parsed = syn::parse_file(&text);
        if let Ok(ast) = &parsed {
            signatures(&ast.items, &mut api);
        }
        let declarations = project::excerpt(&api.join("\n"), (10000 - bytes).min(2500));
        bytes += declarations.len();
        rows.push(json!({"path":path,"reachable_from_default_entrypoints":active.contains(path),"parse_ok":parsed.is_ok(),"api":declarations}));
    }
    Ok(
        json!({"project_files":files.iter().take(200).collect::<Vec<_>>(),"project_files_truncated":files.len()>200,"note":"Project inventory plus optional Rust declarations. An empty Rust index does not mean the project is empty. Use list_files, search and read_file for other formats. Index of on-disk declarations, not claims of correctness. Reachability is a static hint for default Cargo entrypoints; cfg/custom targets may differ. Read source and run checks to verify behavior. Existing types should be reused rather than duplicated.","files":rows}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinguishes_wired_code_from_legacy_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/model")).unwrap();
        std::fs::write(
            d.path().join("src/lib.rs"),
            "#[path=\"model/run.rs\"] pub mod run;",
        )
        .unwrap();
        std::fs::write(
            d.path().join("src/model/run.rs"),
            "pub struct Run { text: String } impl Run { pub fn text(&self)->&str { &self.text } }",
        )
        .unwrap();
        std::fs::write(d.path().join("src/run.rs"), "pub struct OldRun;").unwrap();
        let i = index(d.path()).unwrap();
        let rows = i["files"].as_array().unwrap();
        assert!(rows.iter().any(|r| r["path"] == "src/model/run.rs"
            && r["reachable_from_default_entrypoints"] == true
            && r["api"].as_str().unwrap().contains("fn text")));
        assert!(
            rows.iter()
                .any(|r| r["path"] == "src/run.rs"
                    && r["reachable_from_default_entrypoints"] == false)
        );
    }
}
