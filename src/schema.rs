use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use api::metadata::METADATA;
use heck::ToLowerCamelCase;
use schemars::generate::SchemaSettings;
use schemars::{Schema, SchemaGenerator};
use serde_json::{Map, Value, json};

use crate::config::DragonballConfig;

pub struct Documents {
    pub schema: Schema,
    pub openapi: Value,
}

pub fn generate() -> Result<Documents> {
    let mut input = generator("input", false);
    let mut output = generator("output", true);
    let mut definitions = Map::new();

    for op in METADATA.iter() {
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
        input.subschema_for::<DragonballConfig>(),
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
    for op in METADATA.iter() {
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
