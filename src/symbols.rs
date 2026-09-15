//! Syntax-aware local Rust lookup. Reference candidates are not type-resolved.
use crate::project;
use anyhow::{Context, Result};
use quote::ToTokens;
use serde_json::{Value, json};
use std::path::Path;
use syn::{
    Item,
    visit::{self, Visit},
};

pub fn schema() -> Value {
    json!({"type":"function","function":{"name":"lookup_symbol","description":"Find exact Rust symbol definitions (including private methods) and identifier reference candidates with paths, line numbers and source excerpts. Accepts a name or Owner::method. Results exclude comments/string contents; references are name matches, not type-resolved calls. Follow next_offset for additional matches. Use before creating a duplicate type or guessing an API.","parameters":{"type":"object","properties":{"symbol":{"type":"string"},"mode":{"type":"string","enum":["definitions","references","both"]},"offset":{"type":"integer","minimum":0}},"required":["symbol"]}}})
}
struct Collector<'a> {
    name: &'a str,
    qualifier: Option<&'a str>,
    owner: String,
    definitions: Vec<Value>,
    references: Vec<Value>,
}
impl Collector<'_> {
    fn definition(&mut self, ident: &syn::Ident, kind: &str, signature: String) {
        if ident != self.name || self.qualifier.is_some_and(|q| q != self.owner) {
            return;
        }
        let span = ident.span().start();
        self.definitions.push(json!({"kind":kind,"line":span.line,"column":span.column+1,"owner":self.owner,"signature":project::excerpt(&signature,1200)}));
    }
}
impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        match item {
            Item::Struct(i) => self.definition(
                &i.ident,
                "struct",
                format!(
                    "{} struct {} {}",
                    i.vis.to_token_stream(),
                    i.ident,
                    i.fields.to_token_stream()
                ),
            ),
            Item::Enum(i) => self.definition(
                &i.ident,
                "enum",
                format!("{} enum {}", i.vis.to_token_stream(), i.ident),
            ),
            Item::Trait(i) => self.definition(
                &i.ident,
                "trait",
                format!("{} trait {}", i.vis.to_token_stream(), i.ident),
            ),
            Item::Fn(i) => self.definition(
                &i.sig.ident,
                "function",
                i.sig.to_token_stream().to_string(),
            ),
            Item::Type(i) => self.definition(&i.ident, "type", i.to_token_stream().to_string()),
            Item::Const(i) => self.definition(
                &i.ident,
                "const",
                format!("{}: {}", i.ident, i.ty.to_token_stream()),
            ),
            Item::Static(i) => self.definition(
                &i.ident,
                "static",
                format!("{}: {}", i.ident, i.ty.to_token_stream()),
            ),
            Item::Mod(i) => self.definition(&i.ident, "module", format!("mod {}", i.ident)),
            _ => {}
        }
        visit::visit_item(self, item);
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let owner = std::mem::replace(&mut self.owner, item.self_ty.to_token_stream().to_string());
        visit::visit_item_impl(self, item);
        self.owner = owner;
    }
    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        let owner = std::mem::replace(&mut self.owner, item.ident.to_string());
        visit::visit_item_trait(self, item);
        self.owner = owner;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.definition(
            &item.sig.ident,
            "method",
            item.sig.to_token_stream().to_string(),
        );
        visit::visit_impl_item_fn(self, item);
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.definition(
            &item.sig.ident,
            "trait_method",
            item.sig.to_token_stream().to_string(),
        );
        visit::visit_trait_item_fn(self, item);
    }
    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        if ident == self.name {
            let span = ident.span().start();
            self.references.push(
                json!({"kind":"reference_candidate","line":span.line,"column":span.column+1}),
            );
        }
    }
}
pub fn lookup(root: &Path, args: &Value) -> Result<Value> {
    let symbol = args["symbol"].as_str().context("Missing symbol")?.trim();
    anyhow::ensure!(
        !symbol.is_empty() && symbol.len() <= 200,
        "Supply a symbol name up to 200 bytes"
    );
    let (qualifier, name) = symbol
        .rsplit_once("::")
        .map_or((None, symbol), |(q, n)| (Some(q), n));
    let mode = args["mode"].as_str().unwrap_or("both");
    anyhow::ensure!(
        matches!(mode, "both" | "definitions" | "references"),
        "Unknown lookup mode"
    );
    let offset = match args.get("offset") {
        Some(v) => v.as_u64().context("offset must be nonnegative")? as usize,
        None => 0,
    };
    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut skipped = Vec::new();
    for path in project::inventory(root)?
        .into_iter()
        .filter(|p| p.ends_with(".rs"))
    {
        let Ok(text) = project::read(root, &path) else {
            skipped.push(path);
            continue;
        };
        let Ok(ast) = syn::parse_file(&text) else {
            skipped.push(path);
            continue;
        };
        let mut visitor = Collector {
            name,
            qualifier,
            owner: String::new(),
            definitions: vec![],
            references: vec![],
        };
        visitor.visit_file(&ast);
        // Definition identifiers are not usages. For qualified queries this still gives
        // name-based candidates elsewhere, explicitly labelled rather than claiming resolution.
        visitor.references.retain(|r| {
            !visitor
                .definitions
                .iter()
                .any(|d| d["line"] == r["line"] && d["column"] == r["column"])
        });
        let lines: Vec<_> = text.lines().collect();
        for row in visitor
            .definitions
            .iter_mut()
            .chain(visitor.references.iter_mut())
        {
            row["path"] = json!(path);
            let line = row["line"].as_u64().unwrap_or(1) as usize;
            row["source"] = json!(project::excerpt(
                lines.get(line.saturating_sub(1)).copied().unwrap_or(""),
                500
            ));
        }
        definitions.extend(visitor.definitions);
        references.extend(visitor.references);
    }
    let mut all = Vec::new();
    if mode != "references" {
        all.extend(definitions);
    }
    if mode != "definitions" {
        all.extend(references);
    }
    let total = all.len();
    let mut rows = Vec::new();
    let mut bytes = 0;
    for row in all.into_iter().skip(offset).take(30) {
        bytes += row.to_string().len();
        if bytes > 8500 {
            break;
        }
        rows.push(row);
    }
    let next = offset.saturating_add(rows.len());
    Ok(
        json!({"symbol":symbol,"matches":rows,"total_matches":total,"next_offset":if next<total{Some(next)}else{None},"skipped_files":skipped.into_iter().take(20).collect::<Vec<_>>(),"note":"Syntax lookup includes inactive code. Reference candidates match identifiers, not resolved types; macros are not expanded. Use source and compiler diagnostics to disambiguate."}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_private_methods_and_excludes_comments_and_strings() {
        let d = tempfile::tempdir().unwrap();
        project::write(d.path(),"lib.rs","struct Thing; impl Thing { fn act(&self) {} }\nfn run(t: Thing) { t.act(); let _ = \"act\"; } // act\n").unwrap();
        let result = lookup(d.path(), &json!({"symbol":"Thing::act"})).unwrap();
        let rows = result["matches"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["kind"], "method");
        assert_eq!(rows[0]["owner"], "Thing");
        assert_eq!(rows[1]["line"], 2);
    }
    #[test]
    fn duplicate_definitions_and_pagination_remain_visible() {
        let d = tempfile::tempdir().unwrap();
        for n in 0..35 {
            project::write(d.path(), &format!("m{n}.rs"), "pub struct Run;").unwrap();
        }
        let first = lookup(d.path(), &json!({"symbol":"Run","mode":"definitions"})).unwrap();
        let next = first["next_offset"].as_u64().unwrap();
        let second = lookup(
            d.path(),
            &json!({"symbol":"Run","mode":"definitions","offset":next}),
        )
        .unwrap();
        assert_eq!(first["total_matches"], 35);
        assert_eq!(
            first["matches"].as_array().unwrap().len()
                + second["matches"].as_array().unwrap().len(),
            35
        );
    }
}
