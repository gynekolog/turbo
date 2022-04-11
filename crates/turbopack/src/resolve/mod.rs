use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    mem::{self, take},
};

use anyhow::{anyhow, Result};
use json::JsonValue;
use turbo_tasks::{trace::TraceSlotVcs, util::try_join_all, Value, ValueToString, Vc};
use turbo_tasks_fs::{
    glob::GlobVc,
    util::{join_path, normalize_path, normalize_request},
    FileJsonContent, FileJsonContentVc, FileSystemEntryType, FileSystemPathVc,
};

use crate::{
    asset::AssetVc,
    reference::{AssetReference, AssetReferenceVc},
    resolve::{
        options::{ConditionValue, ResolvedMapVc},
        pattern::{read_matches, Pattern, PatternMatch, PatternVc},
    },
    source_asset::SourceAssetVc,
};

use self::{
    exports::{ExportsField, ExportsValue},
    options::{
        resolve_modules_options, ImportMap, ImportMapResult, ImportMapping, ResolveIntoPackage,
        ResolveModules, ResolveModulesOptionsVc, ResolveOptions, ResolveOptionsVc, ResolvedMap,
    },
    parse::{Request, RequestVc},
    prefix_tree::PrefixTree,
};

mod exports;
pub mod options;
pub mod parse;
pub mod pattern;
mod prefix_tree;

#[derive(PartialEq, Eq, Clone, Debug, TraceSlotVcs)]
pub enum SpecialType {
    OriginalReferenceExternal,
    OriginalRefernceTypeExternal(String),
    Ignore,
    Empty,
    Custom(u8),
}

#[turbo_tasks::value(shared)]
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum ResolveResult {
    Nested(Vec<AssetReferenceVc>),
    Single(AssetVc, Vec<AssetReferenceVc>),
    Keyed(HashMap<String, AssetVc>, Vec<AssetReferenceVc>),
    Alternatives(HashSet<AssetVc>, Vec<AssetReferenceVc>),
    Special(SpecialType, Vec<AssetReferenceVc>),
    Unresolveable(Vec<AssetReferenceVc>),
}

impl Default for ResolveResult {
    fn default() -> Self {
        ResolveResult::Unresolveable(Vec::new())
    }
}

impl ResolveResult {
    pub fn unresolveable() -> Self {
        ResolveResult::Unresolveable(Vec::new())
    }

    pub fn add_reference(&mut self, reference: AssetReferenceVc) {
        match self {
            ResolveResult::Nested(list) => list.push(reference),
            ResolveResult::Single(_, list)
            | ResolveResult::Keyed(_, list)
            | ResolveResult::Alternatives(_, list)
            | ResolveResult::Special(_, list)
            | ResolveResult::Unresolveable(list) => list.push(reference),
        }
    }

    fn get_references(&self) -> &Vec<AssetReferenceVc> {
        match self {
            ResolveResult::Nested(list)
            | ResolveResult::Single(_, list)
            | ResolveResult::Keyed(_, list)
            | ResolveResult::Alternatives(_, list)
            | ResolveResult::Special(_, list)
            | ResolveResult::Unresolveable(list) => list,
        }
    }

    pub fn merge_alternatives(&mut self, other: &ResolveResult) {
        fn extend_option_list<T: Clone>(list: &mut Option<Vec<T>>, other: Option<&Vec<T>>) {
            match (list.as_mut(), other) {
                (Some(list), Some(other)) => list.extend(other.iter().map(|i| i.clone())),
                (Some(_), None) => {}
                (None, None) => {}
                (None, Some(other)) => *list = Some(other.clone()),
            };
        }
        match self {
            ResolveResult::Nested(list) => {
                let list = mem::take(list);
                *self = ResolveResult::Unresolveable(list)
            }
            ResolveResult::Single(asset, list) => {
                *self = ResolveResult::Alternatives(HashSet::from([asset.clone()]), take(list))
            }
            ResolveResult::Keyed(_, list) | ResolveResult::Special(_, list) => {
                *self = ResolveResult::Unresolveable(take(list));
            }
            ResolveResult::Alternatives(_, _) | ResolveResult::Unresolveable(_) => {
                // already is appropriate type
            }
        }
        match self {
            ResolveResult::Nested(_)
            | ResolveResult::Single(_, _)
            | ResolveResult::Keyed(_, _)
            | ResolveResult::Special(_, _) => {
                unreachable!()
            }
            ResolveResult::Alternatives(assets, list) => match other {
                ResolveResult::Single(asset, list2) => {
                    assets.insert(asset.clone());
                    list.extend(list2.iter().cloned());
                }
                ResolveResult::Alternatives(assets2, list2) => {
                    assets.extend(assets2.iter().cloned());
                    list.extend(list2.iter().cloned());
                }
                ResolveResult::Nested(_)
                | ResolveResult::Keyed(_, _)
                | ResolveResult::Special(_, _)
                | ResolveResult::Unresolveable(_) => {
                    list.extend(other.get_references().iter().cloned());
                }
            },
            ResolveResult::Unresolveable(list) => match other {
                ResolveResult::Single(asset, list2) => {
                    list.extend(list2.iter().cloned());
                    *self = ResolveResult::Alternatives(
                        [asset.clone()].into_iter().collect(),
                        take(list),
                    );
                }
                ResolveResult::Alternatives(assets, list2) => {
                    list.extend(list2.iter().cloned());
                    *self = ResolveResult::Alternatives(assets.clone(), take(list));
                }
                ResolveResult::Nested(_)
                | ResolveResult::Keyed(_, _)
                | ResolveResult::Special(_, _)
                | ResolveResult::Unresolveable(_) => {
                    list.extend(other.get_references().iter().cloned());
                }
            },
        }
    }

    pub fn is_unresolveable(&self) -> bool {
        if let ResolveResult::Unresolveable(_) = self {
            true
        } else {
            false
        }
    }

    pub async fn map<A, AF, R, RF>(&self, mut asset_fn: A, reference_fn: R) -> Result<Self>
    where
        A: FnMut(&AssetVc) -> AF,
        AF: Future<Output = Result<AssetVc>>,
        R: FnMut(&AssetReferenceVc) -> RF,
        RF: Future<Output = Result<AssetReferenceVc>>,
    {
        Ok(match self {
            ResolveResult::Nested(refs) => {
                let refs = try_join_all(refs.iter().map(reference_fn)).await?;
                ResolveResult::Nested(refs)
            }
            ResolveResult::Single(asset, refs) => {
                let asset = asset_fn(asset).await?;
                let refs = try_join_all(refs.iter().map(reference_fn)).await?;
                ResolveResult::Single(asset, refs)
            }
            ResolveResult::Keyed(map, refs) => {
                let mut new_map = HashMap::new();
                for (key, value) in map.iter() {
                    new_map.insert(key.clone(), asset_fn(value).await?);
                }
                let refs = try_join_all(refs.iter().map(reference_fn)).await?;
                ResolveResult::Keyed(new_map, refs)
            }
            ResolveResult::Alternatives(assets, refs) => {
                let mut new_assets = HashSet::new();
                for asset in assets.iter() {
                    new_assets.insert(asset_fn(asset).await?);
                }
                let refs = try_join_all(refs.iter().map(reference_fn)).await?;
                ResolveResult::Alternatives(new_assets, refs)
            }
            ResolveResult::Special(ty, refs) => {
                let refs = try_join_all(refs.iter().map(reference_fn)).await?;
                ResolveResult::Special(ty.clone(), refs)
            }
            ResolveResult::Unresolveable(refs) => {
                let refs = try_join_all(refs.iter().map(reference_fn)).await?;
                ResolveResult::Unresolveable(refs)
            }
        })
    }
}

#[turbo_tasks::value_impl]
impl ResolveResultVc {
    pub async fn add_reference(self, reference: AssetReferenceVc) -> Result<Self> {
        let mut this = self.await?.clone();
        this.add_reference(reference);
        Ok(this.into())
    }

    pub async fn alternatives(results: Vec<ResolveResultVc>) -> Result<Self> {
        if results.len() == 1 {
            return Ok(results.into_iter().next().unwrap());
        }
        let mut iter = results.into_iter();
        if let Some(current) = iter.next() {
            let mut current = current.await?.clone();
            for result in iter {
                current.merge_alternatives(&*result.await?);
            }
            Ok(Self::slot(current))
        } else {
            Ok(Self::slot(ResolveResult::Unresolveable(Vec::new())))
        }
    }

    pub async fn is_unresolveable(self) -> Result<Vc<bool>> {
        let this = self.await?.clone();
        Ok(Vc::slot(this.is_unresolveable()))
    }
}

#[turbo_tasks::function]
pub async fn resolve_options(context: FileSystemPathVc) -> Result<ResolveOptionsVc> {
    let parent = context.clone().parent().resolve().await?;
    if parent != context {
        return Ok(resolve_options(parent));
    }
    let context_value = context.get().await?;
    let root = FileSystemPathVc::new(context_value.fs.clone(), "");
    let mut direct_mappings = PrefixTree::new();
    for req in [
        "assert",
        "async_hooks",
        "buffer",
        "child_process",
        "cluster",
        "console",
        "constants",
        "crypto",
        "dgram",
        "diagnostics_channel",
        "dns",
        "dns/promises",
        "domain",
        "events",
        "fs",
        "fs/promises",
        "http",
        "http2",
        "https",
        "inspector",
        "module",
        "net",
        "os",
        "path",
        "path/posix",
        "path/win32",
        "perf_hooks",
        "process",
        "punycode",
        "querystring",
        "readline",
        "repl",
        "stream",
        "stream/promises",
        "stream/web",
        "string_decoder",
        "sys",
        "timers",
        "timers/promises",
        "tls",
        "trace_events",
        "tty",
        "url",
        "util",
        "util/types",
        "v8",
        "vm",
        "wasi",
        "worker_threads",
        "zlib",
        "pnpapi",
    ] {
        direct_mappings.insert(req, ImportMapping::External(None))?;
        direct_mappings.insert(&format!("node:{req}"), ImportMapping::External(None))?;
    }
    let glob_mappings = vec![
        (
            context.clone(),
            GlobVc::new("**/*/next/dist/server/next.js").await?.clone(),
            ImportMapping::Ignore,
        ),
        (
            context.clone(),
            GlobVc::new("**/*/next/dist/bin/next").await?.clone(),
            ImportMapping::Ignore,
        ),
    ];
    let import_map = ImportMap {
        direct: direct_mappings,
        by_glob: Vec::new(),
    }
    .into();
    let resolved_map = ResolvedMap {
        by_glob: glob_mappings,
    }
    .into();
    Ok(ResolveOptions {
        extensions: vec![".js".to_string(), ".node".to_string(), ".json".to_string()],
        modules: vec![ResolveModules::Nested(
            root,
            vec!["node_modules".to_string()],
        )],
        into_package: vec![
            ResolveIntoPackage::ExportsField {
                field: "exports".to_string(),
                conditions: [
                    ("types".to_string(), ConditionValue::Unset),
                    ("react-server".to_string(), ConditionValue::Unset),
                    ("production".to_string(), ConditionValue::Unknown),
                    ("development".to_string(), ConditionValue::Unknown),
                    ("import".to_string(), ConditionValue::Unknown),
                    ("require".to_string(), ConditionValue::Unknown),
                ]
                .into_iter()
                .collect(),
                unspecified_conditions: ConditionValue::Unset,
            },
            ResolveIntoPackage::MainField("main".to_string()),
            ResolveIntoPackage::Default("index".to_string()),
        ],
        import_map: Some(import_map),
        resolved_map: Some(resolved_map),
        ..Default::default()
    }
    .into())
}

async fn exists(fs_path: FileSystemPathVc) -> Result<bool> {
    Ok(
        if let FileSystemEntryType::File = &*fs_path.get_type().await? {
            true
        } else {
            false
        },
    )
}

async fn dir_exists(fs_path: FileSystemPathVc) -> Result<bool> {
    Ok(
        if let FileSystemEntryType::Directory = &*fs_path.get_type().await? {
            true
        } else {
            false
        },
    )
}

#[turbo_tasks::value(shared)]
#[derive(PartialEq, Eq)]
enum ExportsFieldResult {
    Some(#[trace_ignore] ExportsField),
    None,
}

#[turbo_tasks::function]
async fn exports_field(
    package_json: FileJsonContentVc,
    field: &str,
) -> Result<ExportsFieldResultVc> {
    if let FileJsonContent::Content(package_json) = &*package_json.await? {
        let field_value = &package_json[field];
        if let JsonValue::Null = field_value {
            return Ok(ExportsFieldResult::None.into());
        }
        let exports_field: Result<ExportsField> = field_value.try_into();
        match exports_field {
            Ok(exports_field) => Ok(ExportsFieldResult::Some(exports_field).into()),
            Err(err) => {
                // TODO report error to stream
                println!(
                    "{}",
                    err.chain()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join(" -> ")
                );
                Ok(ExportsFieldResult::None.into())
            }
        }
    } else {
        Ok(ExportsFieldResult::None.into())
    }
}

#[turbo_tasks::value(shared)]
#[derive(PartialEq, Eq)]
pub enum FindContextFileResult {
    Found(FileSystemPathVc),
    NotFound,
}

#[turbo_tasks::function]
pub async fn find_context_file(
    context: FileSystemPathVc,
    name: &str,
) -> Result<FindContextFileResultVc> {
    let context_value = context.get().await?;
    if let Some(new_path) = join_path(&context_value.path, name) {
        let fs_path = FileSystemPathVc::new_normalized(context_value.fs.clone(), new_path);
        if exists(fs_path.clone()).await? {
            return Ok(FindContextFileResult::Found(fs_path).into());
        }
    }
    if context_value.is_root() {
        return Ok(FindContextFileResult::NotFound.into());
    }
    Ok(find_context_file(context.parent(), name))
}

#[turbo_tasks::value(shared)]
#[derive(PartialEq, Eq)]
enum FindPackageResult {
    Package(FileSystemPathVc),
    NotFound,
}

#[turbo_tasks::function]
async fn find_package(
    context: FileSystemPathVc,
    package_name: String,
    options: ResolveModulesOptionsVc,
) -> Result<FindPackageResultVc> {
    let options = options.await?;
    for resolve_modules in &options.modules {
        match resolve_modules {
            ResolveModules::Nested(root, names) => {
                let mut context = context.clone();
                let mut context_value = context.get().await?;
                while context_value.is_inside(&*root.get().await?) {
                    for name in names.iter() {
                        if let Some(nested_path) = join_path(&context_value.path, &name) {
                            if let Some(new_path) = join_path(&nested_path, &package_name) {
                                let fs_path = FileSystemPathVc::new_normalized(
                                    context_value.fs.clone(),
                                    nested_path,
                                );
                                if dir_exists(fs_path).await? {
                                    let fs_path = FileSystemPathVc::new_normalized(
                                        context_value.fs.clone(),
                                        new_path,
                                    );
                                    if dir_exists(fs_path.clone()).await? {
                                        return Ok(FindPackageResult::Package(fs_path).into());
                                    }
                                }
                            }
                        }
                    }
                    context = context.parent();
                    let new_context_value = context.get().await?;
                    if *new_context_value == *context_value {
                        break;
                    }
                    context_value = new_context_value;
                }
            }
            ResolveModules::Path(context) => {
                let package_dir = context.clone().join(&package_name);
                if dir_exists(package_dir.clone()).await? {
                    return Ok(FindPackageResult::Package(package_dir.resolve().await?).into());
                }
            }
            ResolveModules::Registry(_, _) => todo!(),
        }
    }
    Ok(FindPackageResult::NotFound.into())
}

fn merge_results(results: Vec<ResolveResultVc>) -> ResolveResultVc {
    match results.len() {
        0 => ResolveResult::unresolveable().into(),
        1 => results.into_iter().next().unwrap(),
        _ => ResolveResultVc::alternatives(results),
    }
}

#[turbo_tasks::function]
pub async fn resolve_raw(
    context: FileSystemPathVc,
    path: PatternVc,
    force_in_context: bool,
) -> Result<ResolveResultVc> {
    fn to_result(path: &FileSystemPathVc) -> ResolveResultVc {
        ResolveResult::Single(SourceAssetVc::new(path.clone()).into(), Vec::new()).into()
    }
    let mut results = Vec::new();
    let pat = path.get().await?;
    if let Some(pat) = pat.filter_could_match("/ROOT/") {
        if let Some(pat) = pat.filter_could_not_match("/ROOT/fsd8nz8og54z") {
            let path = PatternVc::new(pat);
            let matches = read_matches(
                context.clone().root(),
                "/ROOT/".to_string(),
                true,
                path.clone(),
            )
            .await?;
            if matches.len() > 10000 {
                println!(
                    "WARN: resolving abs pattern {} in {} leads to {} results",
                    path.to_string().await?,
                    context.clone().to_string().await?,
                    matches.len()
                );
            } else {
                for m in matches.iter() {
                    if let PatternMatch::File(_, path) = m {
                        results.push(to_result(&path));
                    }
                }
            }
        }
    }
    {
        let matches = read_matches(context.clone(), "".to_string(), force_in_context, path).await?;
        if matches.len() > 10000 {
            println!(
                "WARN: resolving pattern {} in {} leads to {} results",
                pat.to_string(),
                context.clone().to_string().await?,
                matches.len()
            );
        }
        for m in matches.iter() {
            if let PatternMatch::File(_, path) = m {
                results.push(to_result(&path));
            }
        }
    }
    Ok(merge_results(results))
}

#[turbo_tasks::function]
pub async fn resolve(
    context: FileSystemPathVc,
    request: RequestVc,
    options: ResolveOptionsVc,
) -> Result<ResolveResultVc> {
    async fn resolved(
        fs_path: FileSystemPathVc,
        resolved_map: &Option<ResolvedMapVc>,
        options: &ResolveOptionsVc,
    ) -> Result<ResolveResultVc> {
        if let Some(resolved_map) = resolved_map {
            match &*resolved_map.clone().lookup(fs_path.clone()).await? {
                ImportMapResult::Result(result) => {
                    return Ok(result.clone());
                }
                ImportMapResult::Alias(request) => {
                    return Ok(resolve(
                        fs_path.clone().parent(),
                        request.clone(),
                        options.clone(),
                    ));
                }
                ImportMapResult::NoEntry => {}
            }
        }
        return Ok(ResolveResult::Single(SourceAssetVc::new(fs_path).into(), Vec::new()).into());
    }

    fn handle_exports_field(
        package_path: &FileSystemPathVc,
        package_json: &FileSystemPathVc,
        options: &ResolveOptionsVc,
        exports_field: &ExportsField,
        path: &str,
        conditions: &BTreeMap<String, ConditionValue>,
        unspecified_conditions: &ConditionValue,
    ) -> Result<ResolveResultVc> {
        let mut results = Vec::new();
        let mut conditions_state = HashMap::new();
        let values = exports_field
            .lookup(path)
            .collect::<Result<Vec<Cow<'_, ExportsValue>>>>()?;
        for value in values.iter() {
            if value.add_results(
                conditions,
                unspecified_conditions,
                &mut conditions_state,
                &mut results,
            ) {
                break;
            }
        }
        {
            let mut duplicates_set = HashSet::new();
            results.retain(|item| duplicates_set.insert(*item));
        }
        let mut resolved_results = Vec::new();
        for path in results {
            if let Some(path) = normalize_path(path) {
                let request = RequestVc::parse(Value::new(format!("./{}", path).into()));
                resolved_results.push(resolve(package_path.clone(), request, options.clone()));
            }
        }
        // other options do not apply anymore when an exports field exist
        let resolved = merge_results(resolved_results);
        let resolved = resolved
            .add_reference(AffectingResolvingAssetReferenceVc::new(package_json.clone()).into());
        return Ok(resolved);
    }

    async fn resolve_into_folder(
        package_path: &FileSystemPathVc,
        package_json: &Option<(FileJsonContentVc, FileSystemPathVc)>,
        options: &ResolveOptionsVc,
    ) -> Result<ResolveResultVc> {
        let options_value = options.get().await?;
        for resolve_into_package in options_value.into_package.iter() {
            match resolve_into_package {
                ResolveIntoPackage::Default(req) => {
                    let str = "./".to_string()
                        + &normalize_path(&req).ok_or_else(|| {
                            anyhow!(
                                "ResolveIntoPackage::Default can't be used with a request that \
                                 escapes the current directory"
                            )
                        })?;
                    let request = RequestVc::parse(Value::new(str.into()));
                    return Ok(resolve(package_path.clone(), request, options.clone()));
                }
                ResolveIntoPackage::MainField(name) => {
                    if let Some((ref package_json, ref package_json_path)) = package_json {
                        if let FileJsonContent::Content(package_json) = &*package_json.get().await?
                        {
                            if let Some(field_value) = package_json[name].as_str() {
                                let request = RequestVc::parse(Value::new(
                                    normalize_request(&field_value).into(),
                                ));
                                let result =
                                    resolve(package_path.clone(), request, options.clone()).await?;
                                // we are not that strict when a main field fails to resolve
                                // we continue to try other alternatives
                                if !result.is_unresolveable() {
                                    let mut result = result.clone();
                                    result.add_reference(
                                        AffectingResolvingAssetReferenceVc::new(
                                            package_json_path.clone(),
                                        )
                                        .into(),
                                    );
                                    return Ok(result.into());
                                }
                            }
                        }
                    }
                }
                ResolveIntoPackage::ExportsField {
                    field,
                    conditions,
                    unspecified_conditions,
                } => {
                    if let Some((ref package_json, ref package_json_path)) = package_json {
                        if let ExportsFieldResult::Some(exports_field) =
                            &*exports_field(package_json.clone(), field).await?
                        {
                            // other options do not apply anymore when an exports field exist
                            return handle_exports_field(
                                package_path,
                                &package_json_path,
                                &options,
                                exports_field,
                                ".",
                                conditions,
                                unspecified_conditions,
                            );
                        }
                    }
                }
            }
        }
        Ok(ResolveResult::unresolveable().into())
    }

    let options_value = options.get().await?;

    // Apply import mappings if provided
    if let Some(import_map) = &options_value.import_map {
        match &*import_map.clone().lookup(request.clone()).await? {
            ImportMapResult::Result(result) => {
                return Ok(result.clone());
            }
            ImportMapResult::Alias(request) => {
                return Ok(resolve(context, request.clone(), options));
            }
            ImportMapResult::NoEntry => {}
        }
    }

    let request_value = request.get().await?;
    Ok(match &*request_value {
        Request::Dynamic => ResolveResult::unresolveable().into(),
        Request::Alternatives { requests } => {
            let results = requests
                .iter()
                .map(|req| resolve(context.clone(), req.clone(), options.clone()))
                .collect();
            merge_results(results)
        }
        Request::Raw {
            path,
            force_in_context,
        } => {
            let mut results = Vec::new();
            let matches = read_matches(
                context,
                "".to_string(),
                *force_in_context,
                PatternVc::new(path.clone()),
            )
            .get()
            .await?;
            for m in matches.iter() {
                match m {
                    PatternMatch::File(_, path) => {
                        results.push(
                            resolved(path.clone(), &options_value.resolved_map, &options).await?,
                        );
                    }
                    PatternMatch::Directory(_, path) => {
                        let path_value = path.get().await?;
                        let package_json = {
                            if let Some(new_path) = join_path(&path_value.path, "package.json") {
                                let fs_path = FileSystemPathVc::new_normalized(
                                    path_value.fs.clone(),
                                    new_path,
                                );
                                Some((fs_path.clone().read_json(), fs_path))
                            } else {
                                None
                            }
                        };
                        results.push(resolve_into_folder(&path, &package_json, &options).await?);
                    }
                }
            }
            merge_results(results)
        }
        Request::Relative {
            path,
            force_in_context,
        } => {
            let mut patterns = Vec::new();
            patterns.push(path.clone());
            for ext in options_value.extensions.iter() {
                let mut path = path.clone();
                path.push(ext.clone().into());
                patterns.push(path);
            }
            let new_pat = Pattern::alternatives(patterns);

            resolve(
                context,
                RequestVc::raw(Value::new(new_pat), *force_in_context),
                options,
            )
        }
        Request::Module { module, path } => {
            let package = find_package(
                context,
                module.clone(),
                resolve_modules_options(options.clone()),
            );
            match &*package.await? {
                FindPackageResult::Package(package_path) => {
                    let package_path_value = package_path.get().await?;
                    let package_json = {
                        if let Some(new_path) = join_path(&package_path_value.path, "package.json")
                        {
                            let fs_path = FileSystemPathVc::new_normalized(
                                package_path_value.fs.clone(),
                                new_path,
                            );
                            Some((fs_path.clone().read_json(), fs_path))
                        } else {
                            None
                        }
                    };
                    let mut results = Vec::new();
                    if path.is_match("") {
                        results.push(
                            resolve_into_folder(&package_path, &package_json, &options).await?,
                        );
                    }
                    if path.could_match_others("") {
                        for resolve_into_package in options_value.into_package.iter() {
                            match resolve_into_package {
                                ResolveIntoPackage::Default(_)
                                | ResolveIntoPackage::MainField(_) => {
                                    // doesn't affect packages with subpath
                                }
                                ResolveIntoPackage::ExportsField {
                                    field,
                                    conditions,
                                    unspecified_conditions,
                                } => {
                                    if let Some((ref package_json, ref package_json_path)) =
                                        package_json
                                    {
                                        if let ExportsFieldResult::Some(exports_field) =
                                            &*exports_field(package_json.clone(), field).await?
                                        {
                                            if let Some(path) = path.clone().into_string() {
                                                results.push(handle_exports_field(
                                                    package_path,
                                                    &package_json_path,
                                                    &options,
                                                    exports_field,
                                                    &format!(".{path}"),
                                                    conditions,
                                                    unspecified_conditions,
                                                )?);
                                            } else {
                                                todo!(
                                                    "pattern into an exports field is not \
                                                     implemented yet"
                                                );
                                            }
                                            // other options do not apply anymore when an exports
                                            // field exist
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        let mut new_pat = path.clone();
                        new_pat.push_front(".".to_string().into());
                        let relative = RequestVc::relative(Value::new(new_pat), true);
                        results.push(resolve(package_path.clone(), relative, options.clone()));
                    }
                    merge_results(results)
                }
                FindPackageResult::NotFound => ResolveResult::unresolveable().into(),
            }
        }
        Request::ServerRelative { path } => {
            let mut new_pat = path.clone();
            new_pat.push_front(".".to_string().into());
            let relative = RequestVc::relative(Value::new(new_pat), true);
            resolve(context.root(), relative, options.clone())
        }
        Request::Windows { path: _ } => ResolveResult::unresolveable().into(),
        Request::Empty => ResolveResult::unresolveable().into(),
        Request::PackageInternal { path: _ } => ResolveResult::unresolveable().into(),
        Request::Uri {
            protocol: _,
            remainer: _,
        } => ResolveResult::unresolveable().into(),
        Request::Unknown { path: _ } => ResolveResult::unresolveable().into(),
    })
}

#[turbo_tasks::value(AssetReference)]
#[derive(PartialEq, Eq)]
struct AffectingResolvingAssetReference {
    file: FileSystemPathVc,
}

impl AffectingResolvingAssetReferenceVc {
    fn new(file: FileSystemPathVc) -> Self {
        Self::slot(AffectingResolvingAssetReference { file })
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for AffectingResolvingAssetReference {
    fn resolve_reference(&self) -> ResolveResultVc {
        ResolveResult::Single(SourceAssetVc::new(self.file.clone()).into(), Vec::new()).into()
    }
}