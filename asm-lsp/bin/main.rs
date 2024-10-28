use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use asm_lsp::types::LspClient;

use asm_lsp::handle::{
    handle_completion_request, handle_diagnostics, handle_did_change_text_document_notification,
    handle_did_close_text_document_notification, handle_did_open_text_document_notification,
    handle_document_symbols_request, handle_goto_def_request, handle_hover_request,
    handle_references_request, handle_signature_help_request,
};
use asm_lsp::{
    get_compile_cmd_for_path, get_compile_cmds, get_completes, get_include_dirs, get_root_config,
    Arch, Assembler, NameToInfoMaps, RootConfig, TreeStore,
};

use compile_commands::{CompilationDatabase, SourceFile};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as LspTypesNotification,
};
use lsp_types::request::{
    Completion, DocumentDiagnosticRequest, DocumentSymbolRequest, GotoDefinition, HoverRequest,
    References, SignatureHelpRequest,
};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionOptionsCompletionItem,
    DiagnosticOptions, DiagnosticServerCapabilities, HoverProviderCapability, InitializeParams,
    OneOf, PositionEncodingKind, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, WorkDoneProgressOptions,
};

use anyhow::Result;
use log::{error, info};
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_textdocument::TextDocuments;

/// Entry point of the server. Connects to the client, loads documentation resources,
/// and then enters the main loop
///
/// # Errors
///
/// Returns `Err` if the server fails to connect to the lsp client
///
/// # Panics
///
/// Panics if JSON serialization of the server capabilities fails
pub fn main() -> Result<()> {
    // initialisation
    // Set up logging. Because `stdio_transport` gets a lock on stdout and stdin, we must have our
    // logging only write out to stderr.
    flexi_logger::Logger::try_with_str("info")?.start()?;

    // LSP server initialisation
    info!("Starting asm_lsp-{}", env!("CARGO_PKG_VERSION"));

    // Create the transport
    let (connection, _io_threads) = Connection::stdio();

    // specify UTF-16 encoding for compatibility with lsp-textdocument
    let position_encoding = Some(PositionEncodingKind::UTF16);

    // Run the server and wait for the two threads to end (typically by trigger LSP Exit event).
    let hover_provider = Some(HoverProviderCapability::Simple(true));

    let completion_provider = Some(CompletionOptions {
        completion_item: Some(CompletionOptionsCompletionItem {
            label_details_support: Some(true),
        }),
        trigger_characters: Some(vec![String::from("%"), String::from(".")]),
        ..Default::default()
    });

    let definition_provider = Some(OneOf::Left(true));

    let text_document_sync = Some(TextDocumentSyncCapability::Kind(
        TextDocumentSyncKind::INCREMENTAL,
    ));

    let signature_help_provider = Some(SignatureHelpOptions {
        trigger_characters: None,
        retrigger_characters: None,
        work_done_progress_options: WorkDoneProgressOptions {
            work_done_progress: Some(false),
        },
    });

    let references_provider = Some(OneOf::Left(true));

    let diagnostic_provider = Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
        identifier: Some(String::from("asm-lsp")),
        inter_file_dependencies: true,
        workspace_diagnostics: false,
        work_done_progress_options: WorkDoneProgressOptions {
            work_done_progress: None,
        },
    }));

    let capabilities = ServerCapabilities {
        position_encoding,
        hover_provider,
        completion_provider,
        signature_help_provider,
        definition_provider,
        text_document_sync,
        document_symbol_provider: Some(OneOf::Left(true)),
        references_provider,
        diagnostic_provider,
        ..ServerCapabilities::default()
    };
    let server_capabilities = serde_json::to_value(capabilities).unwrap();
    let initialization_params = connection.initialize(server_capabilities)?;

    let params: InitializeParams = serde_json::from_value(initialization_params).unwrap();
    info!("Client initialization params: {:?}", params);
    let mut config = match get_root_config(&params) {
        Ok(cfg) => cfg,
        Err(e) => {
            let err_msg_params = lsp_types::ShowMessageParams {
                typ: lsp_types::MessageType::ERROR,
                message: format!("{e}. Please make corrections and restart asm-lsp."),
            };
            let result = serde_json::to_value(err_msg_params).unwrap();
            let err_notif = lsp_server::Notification {
                method: lsp_types::notification::ShowMessage::METHOD.to_string(),
                params: result,
            };
            connection.sender.send(Message::Notification(err_notif))?;
            // HACK: Sleep so our error message isn't immediately overwritten by
            // the LSP client informing the user that we exited with an error code
            sleep(Duration::from_secs(5));
            std::process::exit(1);
        }
    };
    info!("Server Configuration: {:?}", config);
    if let Some(ref client_info) = params.client_info {
        if client_info.name.eq("helix") {
            info!("Helix LSP client detected");
            config.set_client(LspClient::Helix);
        }
    }

    // Populate names to `Instruction`/`Register`/`Directive` maps
    let mut names_to_info = NameToInfoMaps::default();
    for isa in config.effective_arches() {
        isa.setup_instructions(&mut names_to_info.instructions);
        isa.setup_registers(&mut names_to_info.registers);
    }

    for assembler in config.effective_assemblers() {
        assembler.setup_directives(&mut names_to_info.directives);
    }

    // Use the maps we populated above to generate completion items
    let instr_completion_items = get_completes(
        &names_to_info.instructions,
        Some(CompletionItemKind::OPERATOR),
    );

    let reg_completion_items =
        get_completes(&names_to_info.registers, Some(CompletionItemKind::VARIABLE));

    let directive_completion_items = get_completes(
        &names_to_info.directives,
        Some(CompletionItemKind::OPERATOR),
    );

    let compile_cmds = get_compile_cmds(&params).unwrap_or_default();
    info!("Loaded compile commands: {:?}", compile_cmds);
    let include_dirs = get_include_dirs(&compile_cmds);

    main_loop(
        &connection,
        &config,
        &names_to_info,
        &instr_completion_items,
        &directive_completion_items,
        &reg_completion_items,
        &compile_cmds,
        &include_dirs,
    )?;

    // HACK: the `writer` thread of `connection` hangs on joining more often than
    // not. Need to investigate this further, but for now just skipping the join
    // (and thus allowing the process to exit) is fine
    // _io_threads.join()?;

    info!("Shutting down asm-lsp");
    Ok(())
}

fn main_loop(
    connection: &Connection,
    config: &RootConfig,
    names_to_info: &NameToInfoMaps,
    instruction_completion_items: &[(Arch, CompletionItem)],
    directive_completion_items: &[(Assembler, CompletionItem)],
    register_completion_items: &[(Arch, CompletionItem)],
    compile_cmds: &CompilationDatabase,
    include_dirs: &HashMap<SourceFile, Vec<PathBuf>>,
) -> Result<()> {
    let mut text_store = TextDocuments::new();
    let mut tree_store = TreeStore::new();

    info!("Starting asm-lsp loop...");
    for msg in &connection.receiver {
        let start = std::time::Instant::now();
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    info!("Recieved shutdown request");
                    return Ok(());
                }
                if let Ok((id, params)) = cast_req::<HoverRequest>(req.clone()) {
                    handle_hover_request(
                        connection,
                        id,
                        config.get_config(&params.text_document_position_params.text_document.uri),
                        &params,
                        &text_store,
                        &mut tree_store,
                        names_to_info,
                        include_dirs,
                    )?;
                    info!(
                        "Hover request serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok((id, params)) = cast_req::<Completion>(req.clone()) {
                    handle_completion_request(
                        connection,
                        id,
                        &params,
                        config.get_config(&params.text_document_position.text_document.uri),
                        &text_store,
                        &mut tree_store,
                        instruction_completion_items,
                        directive_completion_items,
                        register_completion_items,
                    )?;
                    info!(
                        "Completion request serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok((id, params)) = cast_req::<GotoDefinition>(req.clone()) {
                    handle_goto_def_request(
                        connection,
                        id,
                        &params,
                        config.get_config(&params.text_document_position_params.text_document.uri),
                        &text_store,
                        &mut tree_store,
                    )?;
                    info!(
                        "Goto definition request serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok((id, params)) = cast_req::<DocumentSymbolRequest>(req.clone()) {
                    handle_document_symbols_request(
                        connection,
                        id,
                        &params,
                        config.get_config(&params.text_document.uri),
                        &text_store,
                        &mut tree_store,
                    )?;
                    info!(
                        "Document symbols request serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok((id, params)) = cast_req::<SignatureHelpRequest>(req.clone()) {
                    handle_signature_help_request(
                        connection,
                        id,
                        &params,
                        config.get_config(&params.text_document_position_params.text_document.uri),
                        &text_store,
                        &mut tree_store,
                        &names_to_info.instructions,
                    )?;
                    info!(
                        "Signature help request serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok((id, params)) = cast_req::<References>(req.clone()) {
                    handle_references_request(
                        connection,
                        id,
                        &params,
                        config.get_config(&params.text_document_position.text_document.uri),
                        &text_store,
                        &mut tree_store,
                    )?;
                    info!(
                        "References request serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok((_id, params)) = cast_req::<DocumentDiagnosticRequest>(req.clone())
                {
                    let project_config = config.get_config(&params.text_document.uri);
                    #[allow(clippy::option_if_let_else)]
                    let cmp_cmds = if let Some(cmd) =
                        get_compile_cmd_for_path(config, &params.text_document.uri)
                    {
                        // If the user provided a compiler invocation command in their config
                        // for the project config covering this file, use it
                        &vec![cmd]
                    } else {
                        // Otherwise pass the default compile commands object
                        compile_cmds
                    };

                    // Ok to unwrap, this should never be `None`
                    if project_config.opts.as_ref().unwrap().diagnostics.unwrap() {
                        handle_diagnostics(
                            connection,
                            &params.text_document.uri,
                            project_config,
                            cmp_cmds,
                        )?;
                        info!(
                            "Diagnostics request serviced in {}ms",
                            start.elapsed().as_millis()
                        );
                    }
                } else {
                    error!("Invalid request format -> {:#?}", req);
                }
            }
            Message::Notification(notif) => {
                if let Ok(params) = cast_notif::<DidOpenTextDocument>(notif.clone()) {
                    handle_did_open_text_document_notification(
                        &params,
                        &mut text_store,
                        &mut tree_store,
                    );
                    info!(
                        "Did open text document notification serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok(params) = cast_notif::<DidChangeTextDocument>(notif.clone()) {
                    handle_did_change_text_document_notification(
                        &params,
                        &mut text_store,
                        &mut tree_store,
                    )?;
                    info!(
                        "Did change text document notification serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok(params) = cast_notif::<DidCloseTextDocument>(notif.clone()) {
                    handle_did_close_text_document_notification(
                        &params,
                        &mut text_store,
                        &mut tree_store,
                    );
                    info!(
                        "Did close text document notification serviced in {}ms",
                        start.elapsed().as_millis()
                    );
                } else if let Ok(params) = cast_notif::<DidSaveTextDocument>(notif.clone()) {
                    let project_config = config.get_config(&params.text_document.uri);
                    // Ok to unwrap, this should never be `None`
                    if project_config.opts.as_ref().unwrap().diagnostics.unwrap() {
                        #[allow(clippy::option_if_let_else)]
                        let cmp_cmds = if let Some(cmd) =
                            get_compile_cmd_for_path(config, &params.text_document.uri)
                        {
                            // If the user provided a compiler invocation command in their config
                            // for the project config covering this file, use it
                            &vec![cmd]
                        } else {
                            // Otherwise pass the default compile commands object
                            compile_cmds
                        };
                        handle_diagnostics(
                            connection,
                            &params.text_document.uri,
                            project_config,
                            cmp_cmds,
                        )?;
                        info!(
                            "Published diagnostics on save in {}ms",
                            start.elapsed().as_millis()
                        );
                    }
                }
            }
            Message::Response(_resp) => {}
        }
    }
    Ok(())
}

fn cast_req<R>(req: Request) -> Result<(RequestId, R::Params)>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    match req.extract(R::METHOD) {
        Ok(value) => Ok(value),
        // Fixme please
        Err(e) => Err(anyhow::anyhow!("Error: {e}")),
    }
}

fn cast_notif<R>(notif: Notification) -> Result<R::Params>
where
    R: lsp_types::notification::Notification,
    R::Params: serde::de::DeserializeOwned,
{
    match notif.extract(R::METHOD) {
        Ok(value) => Ok(value),
        // Fixme please
        Err(e) => Err(anyhow::anyhow!("Error: {e}")),
    }
}