import argparse
import json
import sys
from pathlib import Path


def references(value):
    if isinstance(value, dict):
        if "$ref" in value:
            yield value["$ref"]
        for child in value.values():
            yield from references(child)
    elif isinstance(value, list):
        for child in value:
            yield from references(child)


def verify(directory):
    try:
        import yaml
        from jsonschema import Draft202012Validator
        from jsonschema_path import SchemaPath
        from openapi_spec_validator import OpenAPIV31SpecValidator
        from referencing import Registry, Resource
        from referencing.jsonschema import DRAFT202012
    except ImportError as error:
        raise RuntimeError(
            f"Missing validation dependency: {error}. "
            "Install openapi-spec-validator==0.9.0 in your Python environment "
            "and select it with make schema-verify PYTHON=/path/to/python."
        ) from error

    schema_path = directory.resolve() / "vm.schema.json"
    openapi_path = directory.resolve() / "openapi.yaml"
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    openapi = yaml.safe_load(openapi_path.read_text(encoding="utf-8"))
    Draft202012Validator.check_schema(schema)

    documents = {schema_path.as_uri(): schema, openapi_path.as_uri(): openapi}
    registry = Registry().with_resources(
        (uri, Resource.from_contents(document, default_specification=DRAFT202012))
        for uri, document in documents.items()
    )
    reference_count = 0
    for uri, document in documents.items():
        for reference in references(document):
            registry.resolver(uri).lookup(reference)
            reference_count += 1

    spec = SchemaPath.from_dict(
        openapi,
        base_uri=openapi_path.as_uri(),
        handlers={"file": documents.__getitem__},
    )
    OpenAPIV31SpecValidator(spec).validate()
    print(f"{openapi_path}: OpenAPI 3.1 OK")
    print(f"{schema_path}: JSON Schema Draft 2020-12 OK")
    print(f"{reference_count} references resolved locally")


def main():
    parser = argparse.ArgumentParser(
        description="Validate generated OpenAPI 3.1 and JSON Schema documents locally."
    )
    parser.add_argument(
        "directory", type=Path, help="directory containing both schema documents"
    )
    args = parser.parse_args()
    try:
        verify(args.directory)
    except Exception as error:
        print(f"schema verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
