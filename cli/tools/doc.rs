// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::colors;
use crate::file_fetcher::File;
use crate::flags::Flags;
use crate::get_types;
use crate::program_state::ProgramState;
use crate::write_json_to_stdout;
use crate::write_to_stdout_ignore_sigpipe;
use deno_ast::MediaType;
use deno_core::error::AnyError;
use deno_core::futures::future;
use deno_core::futures::future::FutureExt;
use deno_core::resolve_url_or_path;
use deno_doc as doc;
use deno_graph::create_graph;
use deno_graph::source::LoadFuture;
use deno_graph::source::LoadResponse;
use deno_graph::source::Loader;
use deno_graph::source::Resolver;
use deno_graph::ModuleSpecifier;
use deno_runtime::permissions::Permissions;
use import_map::ImportMap;
use std::path::PathBuf;
use std::sync::Arc;

struct StubDocLoader;

impl Loader for StubDocLoader {
  fn load(
    &mut self,
    specifier: &ModuleSpecifier,
    _is_dynamic: bool,
  ) -> LoadFuture {
    Box::pin(future::ready((specifier.clone(), Ok(None))))
  }
}

#[derive(Debug)]
struct DocResolver {
  import_map: Option<ImportMap>,
}

impl Resolver for DocResolver {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &ModuleSpecifier,
  ) -> Result<ModuleSpecifier, AnyError> {
    if let Some(import_map) = &self.import_map {
      return import_map
        .resolve(specifier, referrer.as_str())
        .map_err(AnyError::from);
    }

    let module_specifier =
      deno_core::resolve_import(specifier, referrer.as_str())?;

    Ok(module_specifier)
  }
}

struct DocLoader {
  program_state: Arc<ProgramState>,
}

impl Loader for DocLoader {
  fn load(
    &mut self,
    specifier: &ModuleSpecifier,
    _is_dynamic: bool,
  ) -> LoadFuture {
    let specifier = specifier.clone();
    let program_state = self.program_state.clone();
    async move {
      let result = program_state
        .file_fetcher
        .fetch(&specifier, &mut Permissions::allow_all())
        .await
        .map(|file| {
          Some(LoadResponse {
            specifier: specifier.clone(),
            content: file.source.clone(),
            maybe_headers: file.maybe_headers,
          })
        });
      (specifier.clone(), result)
    }
    .boxed_local()
  }
}

pub async fn print_docs(
  flags: Flags,
  source_file: Option<String>,
  json: bool,
  maybe_filter: Option<String>,
  private: bool,
) -> Result<(), AnyError> {
  let program_state = ProgramState::build(flags.clone()).await?;
  let source_file = source_file.unwrap_or_else(|| "--builtin".to_string());
  let source_parser = deno_graph::DefaultSourceParser::new();

  let parse_result = if source_file == "--builtin" {
    let mut loader = StubDocLoader;
    let source_file_specifier =
      ModuleSpecifier::parse("deno://lib.deno.d.ts").unwrap();
    let graph = create_graph(
      source_file_specifier.clone(),
      &mut loader,
      None,
      None,
      None,
    )
    .await;
    let doc_parser = doc::DocParser::new(graph, private, &source_parser);
    doc_parser.parse_source(
      &source_file_specifier,
      MediaType::Dts,
      Arc::new(get_types(flags.unstable)),
    )
  } else {
    let module_specifier = resolve_url_or_path(&source_file)?;

    // If the root module has external types, the module graph won't redirect it,
    // so instead create a dummy file which exports everything from the actual file being documented.
    let root_specifier = resolve_url_or_path("./$deno$doc.ts").unwrap();
    let root = File {
      local: PathBuf::from("./$deno$doc.ts"),
      maybe_types: None,
      media_type: MediaType::TypeScript,
      source: Arc::new(format!("export * from \"{}\";", module_specifier)),
      specifier: root_specifier.clone(),
      maybe_headers: None,
    };

    // Save our fake file into file fetcher cache.
    program_state.file_fetcher.insert_cached(root);

    let mut loader = DocLoader {
      program_state: program_state.clone(),
    };
    let resolver = DocResolver {
      import_map: program_state.maybe_import_map.clone(),
    };
    let graph = create_graph(
      root_specifier.clone(),
      &mut loader,
      Some(&resolver),
      None,
      None,
    )
    .await;
    let doc_parser = doc::DocParser::new(graph, private, &source_parser);
    doc_parser.parse_with_reexports(&root_specifier)
  };

  let mut doc_nodes = match parse_result {
    Ok(nodes) => nodes,
    Err(e) => {
      eprintln!("{}", e);
      std::process::exit(1);
    }
  };

  if json {
    write_json_to_stdout(&doc_nodes)
  } else {
    doc_nodes.retain(|doc_node| doc_node.kind != doc::DocNodeKind::Import);
    let details = if let Some(filter) = maybe_filter {
      let nodes =
        doc::find_nodes_by_name_recursively(doc_nodes, filter.clone());
      if nodes.is_empty() {
        eprintln!("Node {} was not found!", filter);
        std::process::exit(1);
      }
      format!(
        "{}",
        doc::DocPrinter::new(&nodes, colors::use_color(), private)
      )
    } else {
      format!(
        "{}",
        doc::DocPrinter::new(&doc_nodes, colors::use_color(), private)
      )
    };

    write_to_stdout_ignore_sigpipe(details.as_bytes()).map_err(AnyError::from)
  }
}
