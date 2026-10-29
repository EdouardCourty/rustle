"""A validator for the subset of JSON Schema (2020-12) the data schemas use,
stdlib only (the Python tools of bench/ have no dependencies).

    from schemalite import validate
    errors = validate(instance, schema)      # [] when valid, else ["/path: message", ...]

Supported keywords: type (name or list), enum, const, required, properties,
additionalProperties (boolean or schema), items, minItems, minimum, minLength,
pattern, format ("uri" only), anyOf, $ref (local "#/$defs/..." only), and the
annotations $schema, $id, $defs, title, description. Any other keyword is an error: a schema cannot
quietly rely on something this check does not enforce (and the website turns
the same schemas into zod validators, which support about the same subset)."""
import re
from urllib.parse import urlsplit

ANNOTATIONS = {"$schema", "$id", "$defs", "title", "description"}
KEYWORDS = ANNOTATIONS | {"type", "enum", "const", "required", "properties", "additionalProperties", "items",
                          "minItems", "minimum", "minLength", "pattern", "format", "anyOf", "$ref"}


def _is(value, name):
    if name == "null":
        return value is None
    if name == "boolean":
        return isinstance(value, bool)
    if name == "integer":
        return (isinstance(value, int) and not isinstance(value, bool)) or (isinstance(value, float) and value.is_integer())
    if name == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if name == "string":
        return isinstance(value, str)
    if name == "array":
        return isinstance(value, list)
    if name == "object":
        return isinstance(value, dict)
    raise ValueError(f"unknown type {name!r}")


def _resolve(root, ref):
    if not ref.startswith("#/"):
        raise ValueError(f"only local references are supported: {ref!r}")
    node = root
    for part in ref[2:].split("/"):
        node = node[part]
    return node


def _check(value, schema, root, path, out):
    unknown = set(schema) - KEYWORDS
    if unknown:
        raise ValueError(f"{path or '/'}: schema keyword(s) not supported by schemalite: {sorted(unknown)}")
    if "$ref" in schema:
        _check(value, _resolve(root, schema["$ref"]), root, path, out)
    if "anyOf" in schema:
        branches = []
        for sub in schema["anyOf"]:
            errs = []
            _check(value, sub, root, path, errs)
            if not errs:
                break
            branches.append(errs)
        else:
            out.append(f"{path or '/'}: matches none of the {len(branches)} alternatives ({branches[0][0]} ...)")
    if "type" in schema:
        names = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(_is(value, n) for n in names):
            out.append(f"{path or '/'}: expected {'|'.join(names)}, got {type(value).__name__}")
            return
    if "const" in schema and (value != schema["const"] or isinstance(value, bool) != isinstance(schema["const"], bool)):
        out.append(f"{path or '/'}: must be {schema['const']!r}, got {value!r}")
    if "enum" in schema and value not in schema["enum"]:
        out.append(f"{path or '/'}: {value!r} is not one of {schema['enum']}")
    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            out.append(f"{path or '/'}: shorter than {schema['minLength']}")
        if schema.get("format") == "uri":
            u = urlsplit(value)
            if not (u.scheme and u.netloc) or any(c.isspace() for c in value):
                out.append(f"{path or '/'}: {value!r} is not a URI")
        elif "format" in schema:
            raise ValueError(f"{path or '/'}: format {schema['format']!r} is not supported by schemalite")
        if "pattern" in schema and not re.search(schema["pattern"], value):
            out.append(f"{path or '/'}: {value!r} does not match {schema['pattern']}")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            out.append(f"{path or '/'}: {value} is below {schema['minimum']}")
    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            out.append(f"{path or '/'}: fewer than {schema['minItems']} items")
        if "items" in schema:
            for i, item in enumerate(value):
                _check(item, schema["items"], root, f"{path}/{i}", out)
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for key in schema.get("required", []):
            if key not in value:
                out.append(f"{path or '/'}: missing required property {key!r}")
        for key, item in value.items():
            if key in props:
                _check(item, props[key], root, f"{path}/{key}", out)
            else:
                extra = schema.get("additionalProperties", True)
                if extra is False:
                    out.append(f"{path or '/'}: unexpected property {key!r}")
                elif isinstance(extra, dict):
                    _check(item, extra, root, f"{path}/{key}", out)


def validate(instance, schema):
    out = []
    _check(instance, schema, schema, "", out)
    return out


def lint(schema, path=""):
    """Problems in a schema itself: keywords schemalite cannot enforce, a property
    without title and description, an object schema that does not say whether
    other properties are allowed. Walks every branch, visited or not."""
    out = []

    def walk(node, where):
        if not isinstance(node, dict):
            return
        bad = set(node) - KEYWORDS
        if bad:
            out.append(f"{where}: unsupported keyword(s) {sorted(bad)}")
        if "properties" in node:
            if "additionalProperties" not in node:
                out.append(f"{where}: an object with properties must set additionalProperties")
            for name, sub in node["properties"].items():
                for need in ("title", "description"):
                    if not (isinstance(sub, dict) and sub.get(need)):
                        out.append(f"{where}/properties/{name}: no {need}")
                walk(sub, f"{where}/properties/{name}")
        if isinstance(node.get("additionalProperties"), dict):
            walk(node["additionalProperties"], f"{where}/additionalProperties")
        if "items" in node:
            walk(node["items"], f"{where}/items")
        for i, sub in enumerate(node.get("anyOf", [])):
            walk(sub, f"{where}/anyOf/{i}")
        for name, sub in node.get("$defs", {}).items():
            walk(sub, f"{where}/$defs/{name}")

    walk(schema, path or "#")
    return out
