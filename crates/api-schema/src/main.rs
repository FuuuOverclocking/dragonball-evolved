use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use api::metadata::OPERATIONS;
use api::{Config, ProcessConfig};
use clap::{Parser, Subcommand};
use heck::ToLowerCamelCase;
use schemars::generate::SchemaSettings;
use schemars::{Schema, SchemaGenerator};
use serde_json::{Map, Value, json};

#[derive(Debug, Parser)]
#[command(about = "Export API documents or validate a configuration without starting a VMM")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Export {
        #[arg(long, default_value = "dist/schema")]
        output_dir: PathBuf,
    },
    Check {
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Export { output_dir } => {
            let documents = generate()?;
            let schema = serde_json::to_string_pretty(&documents.schema)? + "\n";
            let openapi = serde_yaml::to_string(&documents.openapi)?;
            fs::create_dir_all(&output_dir)
                .with_context(|| format!("create {}", output_dir.display()))?;
            for (name, content) in [("vm.schema.json", schema), ("openapi.yaml", openapi)] {
                let path = output_dir.join(name);
                fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
                println!("{}", path.display());
            }
        }
        Command::Check { path } => {
            let config = Config::load(&path)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
    }
    Ok(())
}

struct Documents {
    schema: Schema,
    openapi: Value,
}

fn generate() -> Result<Documents> {
    let mut input = generator("input", false);
    let mut output = generator("output", true);
    let mut definitions = Map::new();

    for op in OPERATIONS.iter() {
        insert_definition(
            &mut definitions,
            format!("request.{}", op.name),
            (op.request)(&mut input),
        )?;
        insert_definition(
            &mut definitions,
            format!("response.{}", op.name),
            (op.response)(&mut output),
        )?;
    }
    insert_definition(
        &mut definitions,
        "Dragonball".into(),
        input.subschema_for::<ProcessConfig>(),
    )?;
    insert_definition(
        &mut definitions,
        "ApiError".into(),
        output.subschema_for::<api::ApiError>(),
    )?;

    let mut schema = api::config::schema(&mut input);
    let properties = schema
        .ensure_object()
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .context("API config schema must contain object properties")?;
    properties["machine"]["description"] = "Virtual machine configuration.".into();
    for (name, property) in [
        (
            "$schema",
            json!({
                "type": "string",
                "description": "Schema location for editor support; never fetched by the VMM."
            }),
        ),
        (
            "extends",
            json!({
                "type": "string",
                "minLength": 1,
                "description": "Parent config path, relative to this file. Objects merge recursively; arrays replace."
            }),
        ),
        ("dragonball", reference("Dragonball")),
    ] {
        if properties.insert(name.into(), property).is_some() {
            bail!("API config binding conflicts with reserved field {name}");
        }
    }
    definitions.insert(
        "input".into(),
        json!({ "$defs": input.take_definitions(false) }),
    );
    definitions.insert(
        "output".into(),
        json!({ "$defs": output.take_definitions(false) }),
    );
    schema.insert("$defs".into(), definitions.into());
    schema.insert("title".into(), "Dragonball VM configuration".into());
    if let Some(dialect) = &input.settings().meta_schema {
        schema.insert("$schema".into(), dialect.as_ref().into());
    }

    let openapi = openapi(&schema)?;
    Ok(Documents { schema, openapi })
}

fn generator(namespace: &str, serialize: bool) -> SchemaGenerator {
    let mut settings = SchemaSettings::draft2020_12();
    settings.definitions_path = format!("/$defs/{namespace}/$defs").into();
    if serialize {
        settings = settings.for_serialize();
    }
    settings.into_generator()
}

fn insert_definition(
    definitions: &mut Map<String, Value>,
    name: String,
    schema: Schema,
) -> Result<()> {
    if definitions
        .insert(name.clone(), schema.to_value())
        .is_some()
    {
        bail!("duplicate schema definition {name}");
    }
    Ok(())
}

fn pointer(name: &str) -> String {
    format!("#/$defs/{}", name.replace('~', "~0").replace('/', "~1"))
}

fn reference(name: &str) -> Value {
    json!({ "$ref": pointer(name) })
}

fn external_reference(name: &str) -> Value {
    json!({ "$ref": format!("./vm.schema.json{}", pointer(name)) })
}

fn openapi(schema: &Schema) -> Result<Value> {
    let mut paths = Map::<String, Value>::new();
    let mut operation_ids = HashSet::new();
    for op in OPERATIONS.iter() {
        let Some(route) = op.route else {
            continue;
        };
        let method = route.method.to_ascii_lowercase();
        if !matches!(
            method.as_str(),
            "get" | "put" | "post" | "delete" | "options" | "head" | "patch" | "trace"
        ) {
            bail!("{}: unsupported OpenAPI method {}", op.name, route.method);
        }
        let operation_id = op.name.to_lower_camel_case();
        if !operation_ids.insert(operation_id.clone()) {
            bail!("duplicate OpenAPI operationId: {operation_id}");
        }
        let request_name = format!("request.{}", op.name);
        let mut operation = json!({
            "operationId": operation_id,
            "requestBody": {
                "required": true,
                "content": { "application/json": { "schema": external_reference(&request_name) } }
            },
            "responses": {
                "200": {
                    "description": "Success",
                    "content": { "application/json": {
                        "schema": external_reference(&format!("response.{}", op.name))
                    } }
                },
                "500": {
                    "description": "Operation failed",
                    "content": { "application/json": { "schema": external_reference("ApiError") } }
                }
            }
        });
        if let Some(description) = description(schema.as_value(), &pointer(&request_name)) {
            operation["description"] = description.into();
        }
        let item = paths.entry(route.path).or_insert_with(|| json!({}));
        if item
            .as_object_mut()
            .expect("path item is an object")
            .insert(method, operation)
            .is_some()
        {
            bail!("duplicate API route: {} {}", route.method, route.path);
        }
    }
    Ok(json!({
        "openapi": "3.1.0",
        "info": { "title": "Dragonball API", "version": env!("CARGO_PKG_VERSION") },
        "paths": paths,
    }))
}

fn description<'a>(document: &'a Value, reference: &'a str) -> Option<&'a str> {
    let mut reference = reference;
    let mut visited = HashSet::new();
    while visited.insert(reference) {
        let node = document.pointer(reference.strip_prefix('#')?)?;
        if let Some(description) = node.get("description").and_then(Value::as_str) {
            return Some(description);
        }
        reference = node.get("$ref")?.as_str()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn command_parses_export_and_check() {
        for (args, expected) in [
            (vec!["api-schema", "export"], "dist/schema"),
            (
                vec!["api-schema", "export", "--output-dir", "output directory"],
                "output directory",
            ),
            (vec!["api-schema", "check", "missing.toml"], "missing.toml"),
        ] {
            let path = match Cli::try_parse_from(args).unwrap().command {
                Command::Export { output_dir } => output_dir,
                Command::Check { path } => path,
            };
            assert_eq!(path, PathBuf::from(expected));
        }
    }

    #[test]
    fn command_rejects_missing_arguments_and_runtime_options() {
        for args in [
            vec!["api-schema"],
            vec!["api-schema", "check"],
            vec!["api-schema", "export", "--output-dir"],
            vec!["api-schema", "unknown"],
            vec!["api-schema", "--config", "missing.toml", "export"],
            vec!["api-schema", "check", "--id", "vm", "vm.toml"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn documents_preserve_dragonball_configuration() {
        let documents = generate().unwrap();
        let schema = documents.schema.as_value();
        assert_eq!(schema["properties"]["dragonball"], reference("Dragonball"));
        assert!(schema["properties"].get("process").is_none());
        let process = schema
            .pointer(
                schema["$defs"]["Dragonball"]["$ref"]
                    .as_str()
                    .unwrap()
                    .strip_prefix('#')
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(process["additionalProperties"], false);
        assert!(process["properties"].get("id").is_some());
        assert!(process["properties"].get("logger").is_some());
        assert_eq!(documents.openapi["openapi"], "3.1.0");
    }
}
