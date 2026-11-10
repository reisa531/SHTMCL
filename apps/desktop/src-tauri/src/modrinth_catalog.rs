//! Validated read models for the Mod catalog. Provider data is never executable HTML.
use crate::modrinth::{identifier, invalid, slug, Modrinth};
use serde_json::{json, Value};
use shtmcl_core::content::{
    error::ContentError,
    loader::{GameTarget, LoaderKind},
    ports::{
        ProjectRef, ProviderIdentityRef, SourceKind, SourcePort, SourceQuery, SourceSearchPage,
    },
    resolve::Candidate,
};
use shtmcl_domain::ErrorEnvelope;
use std::collections::BTreeSet;

pub(crate) fn text<'a>(value: &'a Value, key: &str, max: usize) -> Result<&'a str, ErrorEnvelope> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max && !s.contains('\0'))
        .ok_or_else(invalid)
}
fn array<'a>(value: &'a Value, key: &str, max: usize) -> Result<&'a Vec<Value>, ErrorEnvelope> {
    value[key]
        .as_array()
        .filter(|v| v.len() <= max)
        .ok_or_else(invalid)
}
fn target_valid(target: &GameTarget) -> Result<(), ErrorEnvelope> {
    if target.game_version.is_empty()
        || target.game_version.len() > 256
        || target.game_version.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    Ok(())
}

impl Modrinth {
    pub(crate) fn search_mods(
        &self,
        query: &SourceQuery,
    ) -> Result<SourceSearchPage, ErrorEnvelope> {
        if query.text.len() > 256
            || query.text.chars().any(char::is_control)
            || !(1..=100).contains(&query.limit)
        {
            return Err(invalid());
        }
        let mut facets = vec![
            vec!["project_type:mod".to_string()],
            vec!["client_side:required".into(), "client_side:optional".into()],
        ];
        if let Some(target) = &query.target {
            target_valid(target)?;
            // A vanilla instance has no Mod loader. Never turn it into an unfiltered search.
            if target.loader == LoaderKind::Vanilla {
                return Ok(SourceSearchPage {
                    query_id: query.query_id.clone(),
                    projects: vec![],
                    connected: true,
                });
            }
            facets.push(vec![format!("versions:{}", target.game_version)]);
            facets.push(vec![format!("categories:{}", target.loader.as_str())]);
        }
        let raw = self.get(
            "search",
            &[
                ("query", query.text.trim().into()),
                ("limit", query.limit.to_string()),
                ("facets", json!(facets).to_string()),
            ],
        )?;
        let hits = array(&raw, "hits", query.limit)?;
        let mut projects = Vec::with_capacity(hits.len());
        let mut seen = BTreeSet::new();
        for hit in hits {
            if hit["project_type"] != "mod" {
                return Err(invalid());
            }
            let id = identifier(text(hit, "project_id", 128)?)?;
            if !seen.insert(id) {
                return Err(invalid());
            }
            projects.push(ProjectRef {
                provider: SourceKind::Modrinth,
                project_id: id.into(),
                slug: slug(text(hit, "slug", 128)?)?.into(),
                title: text(hit, "title", 512)?.into(),
            });
        }
        Ok(SourceSearchPage {
            query_id: query.query_id.clone(),
            projects,
            connected: true,
        })
    }

    pub(crate) fn project(&self, project_id: &str) -> Result<Value, ErrorEnvelope> {
        identifier(project_id)?;
        let raw = self.get(&format!("project/{project_id}"), &[])?;
        if raw["id"] != project_id || raw["project_type"] != "mod" {
            return Err(invalid());
        }
        let description = raw["description"]
            .as_str()
            .filter(|s| s.len() <= 4096)
            .ok_or_else(invalid)?;
        let side = raw["client_side"]
            .as_str()
            .filter(|s| ["required", "optional", "unsupported", "unknown"].contains(s))
            .ok_or_else(invalid)?;
        Ok(
            json!({"provider":"modrinth", "projectId":project_id, "slug":slug(text(&raw,"slug",128)?)?,
            "title":text(&raw,"title",512)?, "description":description, "clientSide":side}),
        )
    }

    pub(crate) fn versions(
        &self,
        project_id: &str,
        target: &GameTarget,
        offset: usize,
    ) -> Result<Value, ErrorEnvelope> {
        identifier(project_id)?;
        target_valid(target)?;
        if offset > 4096 {
            return Err(invalid());
        }
        let empty = || json!({"projectId":project_id,"target":target.to_value(),"versions":[],"offset":offset,"total":0});
        if target.loader == LoaderKind::Vanilla {
            return Ok(empty());
        }
        let project = self.project(project_id)?;
        if project["clientSide"] == "unsupported" || project["clientSide"] == "unknown" {
            return Ok(empty());
        }
        let raw = self.get(
            &format!("project/{project_id}/version"),
            &[
                ("game_versions", json!([target.game_version]).to_string()),
                ("loaders", json!([target.loader.as_str()]).to_string()),
            ],
        )?;
        let versions = raw
            .as_array()
            .filter(|v| v.len() <= 4096)
            .ok_or_else(invalid)?;
        let mut checked = Vec::new();
        let mut seen = BTreeSet::new();
        for version in versions {
            if version["project_id"] != project_id {
                return Err(invalid());
            }
            // Filter again locally; server facet matching does not prove file compatibility.
            let games = array(version, "game_versions", 4096)?;
            let loaders = array(version, "loaders", 64)?;
            if !games.iter().all(Value::is_string) || !loaders.iter().all(Value::is_string) {
                return Err(invalid());
            }
            if !games.iter().any(|v| v == &target.game_version)
                || !loaders.iter().any(|v| v == target.loader.as_str())
            {
                continue;
            }
            let id = identifier(text(version, "id", 128)?)?;
            if !seen.insert(id) {
                return Err(invalid());
            }
            checked.push(version_projection(version)?);
        }
        // Modrinth publishes ISO-8601 timestamps. Retain upstream newest-first order.
        let total = checked.len();
        let page: Vec<_> = checked.into_iter().skip(offset).take(20).collect();
        Ok(
            json!({"projectId":project_id,"target":target.to_value(),"versions":page,"offset":offset,"total":total}),
        )
    }
}

fn version_projection(version: &Value) -> Result<Value, ErrorEnvelope> {
    let kind = text(version, "version_type", 16)?;
    if !["release", "beta", "alpha"].contains(&kind) {
        return Err(invalid());
    }
    let files = array(version, "files", 64)?;
    if files.is_empty() {
        return Err(invalid());
    }
    let mut projected_files = Vec::new();
    for file in files {
        let name = text(file, "filename", 255)?;
        // These are display-only names, never filesystem destinations.
        let size = file["size"]
            .as_u64()
            .filter(|s| *s <= 2 * 1024 * 1024 * 1024)
            .ok_or_else(invalid)?;
        projected_files.push(json!({"name":name,"bytes":size,"primary":file["primary"].as_bool().ok_or_else(invalid)?}));
    }
    let mut dependencies = Vec::new();
    for dependency in array(version, "dependencies", 256)? {
        let kind = text(dependency, "dependency_type", 32)?;
        if !["required", "optional", "incompatible", "embedded"].contains(&kind) {
            return Err(invalid());
        }
        let mut entry = json!({"kind":kind});
        for (upstream, local) in [("project_id", "projectId"), ("version_id", "versionId")] {
            if !dependency[upstream].is_null() {
                entry[local] = json!(identifier(text(dependency, upstream, 128)?)?);
            }
        }
        if let Some(name) = dependency["file_name"].as_str().filter(|s| s.len() <= 255) {
            entry["fileName"] = json!(name);
        }
        // Preserve unresolved metadata explicitly; displaying it is not solving it.
        dependencies.push(entry);
    }
    Ok(
        json!({"versionId":identifier(text(version,"id",128)?)?,"name":text(version,"name",512)?,
        "versionNumber":text(version,"version_number",512)?,"kind":kind,
        "published":text(version,"date_published",64)?,"files":projected_files,"dependencies":dependencies}),
    )
}

impl SourcePort for Modrinth {
    fn search(&self, query: &SourceQuery) -> Result<SourceSearchPage, ContentError> {
        if query.provider != Some(SourceKind::Modrinth) {
            return Err(ContentError::OwnerUnavailable { owner: "content" });
        }
        self.search_mods(query)
            .map_err(|_| ContentError::SourceUnavailable {
                provider: "modrinth".into(),
            })
    }
    fn candidates(
        &self,
        _project: &ProjectRef,
        _target: &GameTarget,
    ) -> Result<Vec<Candidate>, ContentError> {
        // Raw version labels are not semver constraints or a solved dependency graph.
        Err(ContentError::OwnerUnavailable {
            owner: "modrinth-dependency-resolution",
        })
    }
    fn identity(&self, provider: SourceKind) -> ProviderIdentityRef {
        ProviderIdentityRef::unlinked(provider)
    }
}
