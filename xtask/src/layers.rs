//! Enforces workspace dependency layering, UI isolation below L5, and the 2D kernel seam.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cargo_metadata::{DependencyKind, MetadataCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Layer(u8),
    Testkit,
    Exempt,
}

pub const TABLE: &[(&str, Class)] = &[
    ("layercraft-geom", Class::Layer(0)),
    ("layercraft-clip", Class::Layer(1)),
    ("layercraft-mesh", Class::Layer(1)),
    ("layercraft-config", Class::Layer(2)),
    ("layercraft-slice", Class::Layer(2)),
    ("layercraft-gcode", Class::Layer(2)),
    ("layercraft-io", Class::Layer(3)),
    ("layercraft-engine", Class::Layer(4)),
    ("layercraft-viewport", Class::Layer(5)),
    ("layercraft-viewport-gpu", Class::Layer(5)),
    ("layercraft-ui-egui", Class::Layer(5)),
    ("layercraft-mcp", Class::Layer(5)),
    ("layercraft-testkit", Class::Testkit),
    ("layercraft", Class::Exempt),
    ("layercraft-cli", Class::Exempt),
    ("layercraft-web", Class::Exempt),
    ("xtask", Class::Exempt),
];

/// Intra-layer dependency orderings allowed within the same layer number.
const INTRA_LAYER_ORDER: &[(&str, &str)] = &[
    ("layercraft-mesh", "layercraft-clip"),
    ("layercraft-slice", "layercraft-config"),
    ("layercraft-gcode", "layercraft-config"),
    ("layercraft-gcode", "layercraft-slice"),
    ("layercraft-viewport-gpu", "layercraft-viewport"),
    ("layercraft-ui-egui", "layercraft-viewport"),
    ("layercraft-ui-egui", "layercraft-viewport-gpu"),
    ("layercraft-mcp", "layercraft-viewport"),
];

const FORBIDDEN_UI_BELOW_L5: &[&str] = &["egui", "eframe", "wgpu", "winit", "egui-wgpu"];
const KERNEL_ONLY_CRATES: &[&str] = &["clipper2-rust", "i_overlay", "boostvoronoi"];

pub fn run(root: &Path) -> Result<(), String> {
    let metadata = MetadataCommand::new()
        .manifest_path(root.join("Cargo.toml"))
        .exec()
        .map_err(|e| format!("cargo metadata failed: {e}"))?;

    let mut class_map: BTreeMap<String, Class> = BTreeMap::new();
    for &(name, cls) in TABLE {
        class_map.insert(name.to_owned(), cls);
    }

    let workspace_ids: BTreeSet<_> = metadata.workspace_members.iter().cloned().collect();
    let mut id_to_pkg = BTreeMap::new();
    for pkg in &metadata.packages {
        id_to_pkg.insert(pkg.id.clone(), pkg);
        if workspace_ids.contains(&pkg.id) && !class_map.contains_key(pkg.name.as_str()) {
            return Err(format!(
                "unclassified workspace crate `{}` — add it to xtask/src/layers.rs TABLE",
                pkg.name
            ));
        }
    }

    let Some(resolve) = metadata.resolve.as_ref() else {
        return Err("cargo metadata did not return a dependency graph".to_owned());
    };
    let mut node_deps: BTreeMap<String, Vec<(String, Vec<DependencyKind>)>> = BTreeMap::new();
    for node in &resolve.nodes {
        let Some(pkg) = id_to_pkg.get(&node.id) else {
            continue;
        };
        let mut deps = Vec::new();
        for dep in &node.deps {
            if let Some(target_pkg) = id_to_pkg.get(&dep.pkg) {
                let kinds: Vec<DependencyKind> = dep.dep_kinds.iter().map(|dk| dk.kind).collect();
                deps.push((target_pkg.name.to_string(), kinds));
            }
        }
        node_deps.insert(pkg.name.to_string(), deps);
    }

    let mut edges = Vec::new();
    for pkg in &metadata.packages {
        if !workspace_ids.contains(&pkg.id) {
            continue;
        }
        for dep in &pkg.dependencies {
            edges.push((pkg.name.to_string(), dep.name.clone(), dep.kind));
        }
    }

    validate_rules(&class_map, &edges, &node_deps)?;
    println!(
        "layers: OK ({} workspace crates checked, seam & UI isolation intact)",
        workspace_ids.len()
    );
    Ok(())
}

pub fn validate_rules(
    class_map: &BTreeMap<String, Class>,
    edges: &[(String, String, DependencyKind)],
    node_deps: &BTreeMap<String, Vec<(String, Vec<DependencyKind>)>>,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for (from, to, kind) in edges {
        let Some(&from_cls) = class_map.get(from) else {
            continue;
        };

        // Testkit isolation check
        if to == "layercraft-testkit"
            && *kind != DependencyKind::Development
            && from_cls != Class::Exempt
        {
            errors.push(format!(
                "`{from}` depends on `layercraft-testkit` outside [dev-dependencies]"
            ));
        }

        // Kernel seam check (direct dependency)
        if KERNEL_ONLY_CRATES.contains(&to.as_str()) && from != "layercraft-clip" {
            errors.push(format!(
                "kernel seam violation: `{from}` directly depends on `{to}` (only `layercraft-clip` may)"
            ));
        }

        // Layered crate ordering
        if let (Class::Layer(fl), Some(&to_cls)) = (from_cls, class_map.get(to)) {
            match to_cls {
                Class::Layer(tl) => {
                    if tl > fl {
                        errors.push(format!(
                            "layer violation: `{from}` (L{fl}) depends upward on `{to}` (L{tl})"
                        ));
                    } else if tl == fl
                        && *kind != DependencyKind::Development
                        && !INTRA_LAYER_ORDER.contains(&(from.as_str(), to.as_str()))
                    {
                        errors.push(format!(
                            "intra-layer violation: `{from}` (L{fl}) cannot depend on sibling `{to}`"
                        ));
                    }
                }
                Class::Testkit if *kind != DependencyKind::Development => {
                    errors.push(format!(
                        "`{from}` (L{fl}) depends on testkit `{to}` in normal dependencies"
                    ));
                }
                Class::Exempt => {
                    errors.push(format!(
                        "layer violation: `{from}` (L{fl}) depends on exempt crate `{to}`"
                    ));
                }
                Class::Testkit => {}
            }
        }
    }

    // Transitive UI/GPU isolation check for all crates <= L4
    for (name, &cls) in class_map {
        if let Class::Layer(l) = cls
            && l <= 4
        {
            let reachable = collect_normal_deps(name, node_deps);
            for ui_crate in FORBIDDEN_UI_BELOW_L5 {
                if reachable.contains(*ui_crate) {
                    errors.push(format!(
                        "UI isolation violation: `{name}` (L{l}) transitively reaches `{ui_crate}`"
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn collect_normal_deps(
    root: &str,
    node_deps: &BTreeMap<String, Vec<(String, Vec<DependencyKind>)>>,
) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut stack = vec![root.to_owned()];
    while let Some(cur) = stack.pop() {
        if let Some(deps) = node_deps.get(&cur) {
            for (dep_name, kinds) in deps {
                let is_normal = kinds.is_empty() || kinds.contains(&DependencyKind::Normal);
                if is_normal && visited.insert(dep_name.clone()) {
                    stack.push(dep_name.clone());
                }
            }
        }
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_upward_layer_and_seam_violations() {
        let mut map = BTreeMap::new();
        map.insert("layercraft-geom".to_owned(), Class::Layer(0));
        map.insert("layercraft-clip".to_owned(), Class::Layer(1));
        map.insert("layercraft-mesh".to_owned(), Class::Layer(1));

        let bad_upward = vec![(
            "layercraft-geom".to_owned(),
            "layercraft-mesh".to_owned(),
            DependencyKind::Normal,
        )];
        assert!(validate_rules(&map, &bad_upward, &BTreeMap::new()).is_err());

        let bad_seam = vec![(
            "layercraft-mesh".to_owned(),
            "clipper2-rust".to_owned(),
            DependencyKind::Normal,
        )];
        assert!(validate_rules(&map, &bad_seam, &BTreeMap::new()).is_err());
    }
}
