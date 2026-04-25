import ast
import pytest
import c_two as cc
from c_two.crm.template import (
    generate_crm_template,
    _format_arguments,
    _format_return_annotation,
)
from tests.fixtures.ihello import Hello


# ── helpers ──────────────────────────────────────────────────────────

def _generate_and_parse(tmp_path):
    """Generate template for Hello and return (source_text, ast_tree)."""
    output = tmp_path / "hello_crm.py"
    generate_crm_template(Hello, output)
    source = output.read_text()
    return source, ast.parse(source)


def _get_class_node(tree, class_name):
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == class_name:
            return node
    return None


def _get_method_names(class_node):
    return [
        node.name
        for node in class_node.body
        if isinstance(node, ast.FunctionDef)
    ]


def _get_method_node(class_node, method_name):
    for node in class_node.body:
        if isinstance(node, ast.FunctionDef) and node.name == method_name:
            return node
    return None


# ── generate_crm_template ───────────────────────────────────────────

class TestGenerateCrmTemplate:
    def test_generates_valid_python(self, tmp_path):
        source, _ = _generate_and_parse(tmp_path)
        # ast.parse raises SyntaxError on invalid code
        ast.parse(source)

    def test_class_name_appends_impl(self, tmp_path):
        """Generated resource class name is always ContractName + 'Impl'."""
        _, tree = _generate_and_parse(tmp_path)
        class_names = [
            n.name for n in ast.walk(tree) if isinstance(n, ast.ClassDef)
        ]
        assert "HelloImpl" in class_names

    def test_contains_all_public_methods(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        assert cls is not None
        method_names = _get_method_names(cls)
        expected = {"greeting", "add", "echo_none", "get_items", "get_data"}
        assert expected == set(method_names)

    def test_greeting_signature(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        method = _get_method_node(cls, "greeting")
        assert method is not None

        # Parameters: self, name: str
        arg_names = [a.arg for a in method.args.args]
        assert arg_names == ["self", "name"]
        assert ast.unparse(method.args.args[1].annotation) == "str"

        # Return type: str
        assert method.returns is not None
        assert ast.unparse(method.returns) == "str"

    def test_add_signature(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        method = _get_method_node(cls, "add")
        assert method is not None

        arg_names = [a.arg for a in method.args.args]
        assert arg_names == ["self", "a", "b"]
        assert ast.unparse(method.args.args[1].annotation) == "int"
        assert ast.unparse(method.args.args[2].annotation) == "int"
        assert ast.unparse(method.returns) == "int"

    def test_echo_none_return_type(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        method = _get_method_node(cls, "echo_none")
        assert method is not None
        ret = ast.unparse(method.returns)
        assert ret == "str | None"

    def test_get_items_signature(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        method = _get_method_node(cls, "get_items")
        assert method is not None

        arg_names = [a.arg for a in method.args.args]
        assert arg_names == ["self", "ids"]
        assert ast.unparse(method.args.args[1].annotation) == "list[int]"
        assert ast.unparse(method.returns) == "list[str]"

    def test_get_data_signature(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        method = _get_method_node(cls, "get_data")
        assert method is not None

        assert ast.unparse(method.args.args[1].annotation) == "int"
        assert ast.unparse(method.returns) == "HelloData"

    def test_output_file_is_written(self, tmp_path):
        output = tmp_path / "subdir" / "hello_crm.py"
        generate_crm_template(Hello, output)
        assert output.exists()
        assert output.stat().st_size > 0

    def test_generated_class_inherits_icrm(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        assert cls is not None
        base_names = [ast.unparse(b) for b in cls.bases]
        assert "Hello" in base_names

    def test_generated_class_has_crm_decorator(self, tmp_path):
        _, tree = _generate_and_parse(tmp_path)
        cls = _get_class_node(tree, "HelloImpl")
        assert cls is not None
        decorator_sources = [ast.unparse(d) for d in cls.decorator_list]
        assert any(d.startswith("cc.crm") for d in decorator_sources)


# ── validation ───────────────────────────────────────────────────────

class TestGenerateCrmTemplateValidation:
    def test_non_crm_raises(self, tmp_path):
        class NotACRM:
            pass

        with pytest.raises(ValueError):
            generate_crm_template(NotACRM, tmp_path / "out.py")

    def test_class_without_direction_raises(self, tmp_path):
        class AlmostCRM:
            direction = "<-"

        with pytest.raises(ValueError):
            generate_crm_template(AlmostCRM, tmp_path / "out.py")


# ── helper functions ─────────────────────────────────────────────────

class TestFormatArguments:
    def _make_arg(self, name, annotation=None):
        arg = ast.arg(arg=name)
        if annotation is not None:
            arg.annotation = annotation
        return arg

    def test_self_only(self):
        args = ast.arguments(
            posonlyargs=[],
            args=[self._make_arg("self")],
            vararg=None,
            kwonlyargs=[],
            kw_defaults=[],
            kwarg=None,
            defaults=[],
        )
        assert _format_arguments(args) == ""

    def test_simple_typed_args(self):
        args = ast.arguments(
            posonlyargs=[],
            args=[
                self._make_arg("self"),
                self._make_arg("name", ast.Constant(value="str")),
            ],
            vararg=None,
            kwonlyargs=[],
            kw_defaults=[],
            kwarg=None,
            defaults=[],
        )
        result = _format_arguments(args)
        assert "name" in result

    def test_with_default_value(self):
        args = ast.arguments(
            posonlyargs=[],
            args=[
                self._make_arg("self"),
                self._make_arg("x", ast.Name(id="int")),
            ],
            vararg=None,
            kwonlyargs=[],
            kw_defaults=[],
            kwarg=None,
            defaults=[ast.Constant(value=42)],
        )
        result = _format_arguments(args)
        assert "x: int = 42" == result

    def test_varargs(self):
        args = ast.arguments(
            posonlyargs=[],
            args=[self._make_arg("self")],
            vararg=ast.arg(arg="args", annotation=ast.Name(id="int")),
            kwonlyargs=[],
            kw_defaults=[],
            kwarg=None,
            defaults=[],
        )
        assert _format_arguments(args) == "*args: int"

    def test_kwargs(self):
        args = ast.arguments(
            posonlyargs=[],
            args=[self._make_arg("self")],
            vararg=None,
            kwonlyargs=[],
            kw_defaults=[],
            kwarg=ast.arg(arg="kwargs", annotation=ast.Name(id="str")),
            defaults=[],
        )
        assert _format_arguments(args) == "**kwargs: str"


class TestFormatReturnAnnotation:
    def test_with_annotation(self):
        ret = ast.Name(id="int")
        assert _format_return_annotation(ret) == " -> int"

    def test_without_annotation(self):
        assert _format_return_annotation(None) == ""
