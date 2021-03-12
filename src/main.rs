use anyhow::{Context, Result};
use arbitrary::{Arbitrary, Unstructured};
use argh::FromArgs;
use openapi_utils::{ReferenceOrExt, SpecExt};
use openapiv3::*;
use rand::{distributions::Alphanumeric, Rng};
use serde_json;
use std::path::PathBuf;
use ureq::OrAnyStatus;
use url::Url;

#[derive(FromArgs, Debug)]
/// OpenAPI fuzzer
struct Args {
    /// path to OpenAPI specification
    #[argh(option, short = 's')]
    spec: PathBuf,

    /// url of api to fuzz
    #[argh(option, short = 'u')]
    url: Url,
}

#[derive(Debug)]
struct Payload<'a> {
    method: &'a str,
    path: &'a str,
    query_params: Vec<(&'a str, String)>,
    path_params: Vec<(&'a str, String)>,
    headers: Vec<(&'a str, String)>,
    cookies: Vec<(&'a str, String)>,
    body: Vec<serde_json::Value>,
    responses: &'a Responses,
}

fn send_request(url: &Url, payload: &Payload) -> Result<ureq::Response> {
    let mut path_with_params = payload.path.to_owned();
    for (name, value) in payload.path_params.iter() {
        path_with_params = path_with_params.replace(&format!("{{{}}}", name), &value);
    }

    let mut request = ureq::request_url(payload.method, &url.join(&path_with_params)?);

    for (param, value) in payload.query_params.iter() {
        request = request.query(param, &value)
    }

    for (header, value) in payload.headers.iter() {
        request = request.set(header, &value)
    }

    if payload.body.len() > 0 {
        Ok(request.send_json(payload.body[0].clone()).or_any_status()?)
    } else {
        Ok(request.call().or_any_status()?)
    }
}

fn generate_json_object(object: &ObjectType, gen: &mut Unstructured) -> Result<serde_json::Value> {
    let mut json_object = serde_json::Map::with_capacity(object.properties.len());
    for (name, schema) in &object.properties {
        let schema_kind = &schema.to_item_ref().schema_kind;
        json_object.insert(name.clone(), schema_kind_to_json(schema_kind, gen)?);
    }
    Ok(serde_json::Value::Object(json_object))
}

fn generate_json_array(array: &ArrayType, gen: &mut Unstructured) -> Result<serde_json::Value> {
    let items = array.items.to_item_ref();
    let (min, max) = (array.min_items.unwrap_or(1), array.max_items.unwrap_or(10));
    let json_array = (min..=max)
        .map(|_| schema_kind_to_json(&items.schema_kind, gen))
        .collect::<Result<Vec<serde_json::Value>>>();
    Ok(serde_json::Value::Array(json_array?))
}

fn schema_type_to_json(schema_type: &Type, gen: &mut Unstructured) -> Result<serde_json::Value> {
    match schema_type {
        Type::String(_string_type) => Ok(ureq::json!(String::arbitrary(gen)?)),
        Type::Number(_number_type) => Ok(ureq::json!(f64::arbitrary(gen)?)),
        Type::Integer(_integer_type) => Ok(ureq::json!(i64::arbitrary(gen)?)),
        Type::Object(object_type) => generate_json_object(object_type, gen),
        Type::Array(array_type) => generate_json_array(array_type, gen),
        Type::Boolean {} => Ok(ureq::json!(bool::arbitrary(gen)?)),
    }
}

fn schema_kind_to_json(
    schema_kind: &SchemaKind,
    gen: &mut Unstructured,
) -> Result<serde_json::Value> {
    match schema_kind {
        SchemaKind::Any(_any) => todo!(),
        SchemaKind::Type(schema_type) => Ok(schema_type_to_json(schema_type, gen)?),
        SchemaKind::OneOf { .. } => todo!(),
        SchemaKind::AnyOf { .. } => todo!(),
        SchemaKind::AllOf { .. } => todo!(),
    }
}

fn prepare_request<'a>(
    method: &'a str,
    path: &'a str,
    operation: &'a Operation,
) -> Result<Payload<'a>> {
    let mut query_params: Vec<(&str, String)> = Vec::new();
    let mut path_params: Vec<(&str, String)> = Vec::new();
    let mut headers: Vec<(&str, String)> = Vec::new();
    let mut cookies: Vec<(&str, String)> = Vec::new();

    // Set-up random data generator
    let fuzzer_input: Vec<u8> = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(1024)
        .collect();
    let mut generator = Unstructured::new(&fuzzer_input);

    for ref_or_param in operation.parameters.iter() {
        match ref_or_param.to_item_ref() {
            Parameter::Query { parameter_data, .. } => {
                query_params.push((&parameter_data.name, String::arbitrary(&mut generator)?))
            }
            Parameter::Path { parameter_data, .. } => {
                path_params.push((&parameter_data.name, String::arbitrary(&mut generator)?))
            }
            Parameter::Header { parameter_data, .. } => {
                headers.push((&parameter_data.name, String::arbitrary(&mut generator)?))
            }
            Parameter::Cookie { parameter_data, .. } => {
                cookies.push((&parameter_data.name, String::arbitrary(&mut generator)?))
            }
        }
    }

    let body = operation.request_body.as_ref().map(|ref_or_body| {
        let request_body = ref_or_body.to_item_ref();
        request_body
            .content
            .iter()
            .map(|(_, media)| {
                media.schema.as_ref().map(|schema| {
                    schema_kind_to_json(&schema.to_item_ref().schema_kind, &mut generator)
                })
            })
            .flatten()
            .collect::<Result<Vec<_>>>()
    });

    Ok(Payload {
        method,
        path,
        query_params,
        path_params,
        headers,
        cookies,
        body: body.unwrap_or(Ok(Vec::new()))?,
        responses: &operation.responses,
    })
}

fn check_response(resp: &ureq::Response, payload: &Payload) {
    print!(".");
    if !payload
        .responses
        .responses
        .contains_key(&StatusCode::Code(resp.status()))
    {
        println!(
            "Unexpected status code: {}\nResponse {:?}",
            resp.status(),
            resp
        );
    }
}

fn create_fuzz_payload<'a>(path: &'a str, item: &'a PathItem) -> Result<Vec<Payload<'a>>> {
    // TODO: Pass parameters to fuzz operation
    let operations = vec![
        ("GET", &item.get),
        ("PUT", &item.put),
        ("POST", &item.post),
        ("DELETE", &item.delete),
        ("OPTIONS", &item.options),
        ("HEAD", &item.head),
        ("PATCH", &item.patch),
        ("TRACE", &item.trace),
    ];

    let mut payloads = Vec::new();
    for (method, op) in operations {
        if let Some(operation) = op {
            payloads.push(prepare_request(method, path, operation)?)
        }
    }

    Ok(payloads)
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let specfile = std::fs::read_to_string(&args.spec)?;
    let openapi_schema: OpenAPI =
        serde_yaml::from_str(&specfile).context("Failed to parse schema")?;
    let openapi_schema = openapi_schema.deref_all();

    loop {
        for (path, ref_or_item) in openapi_schema.paths.iter() {
            let item = ref_or_item.to_item_ref();
            for payload in create_fuzz_payload(path, item)? {
                match send_request(&args.url, &payload) {
                    Ok(resp) => check_response(&resp, &payload),
                    Err(e) => eprintln!("Err sending req: {}", e),
                };
            }
        }
    }
}
