import sys
import ast
import inspect
import logging
from pathlib import Path
from typing import Type, TypeVar, get_type_hints, get_origin, get_args

T = TypeVar('T')

logger = logging.getLogger(__name__)

def generate_crm_template(icrm_class: Type[T], output_path: str | Path) -> None:
    """
    Generate a CRM template based on an ICRM class.

    Args:
        icrm_class (Type[T]): The ICRM class to generate a template from
        output_path (str | Path): Path where the template file will be saved
    """
    
    # Validate that the class is an ICRM
    if not hasattr(icrm_class, 'direction') or icrm_class.direction != '->':
        raise ValueError(f'{icrm_class.__name__} is not a valid ICRM class (decorated with @icrm)')
    
    # Get source file path
    source_file = inspect.getsourcefile(icrm_class)
    
    # Parse source code using AST
    with open(source_file, 'r') as f:
        source_code = f.read()
    tree = ast.parse(source_code)
    
    # Find the ICRM class definition
    icrm_class_node: ast.ClassDef | None = None
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == icrm_class.__name__:
            icrm_class_node = node
            break
    
    if not icrm_class_node:
        raise ValueError(f'Could not find class {icrm_class.__name__} in source file {source_file}')
    
    # Extract method information using AST
    methods = _extract_method_from_ast(icrm_class_node)
    
    # Extract transferable dependencies
    transferable_deps = _extract_transferable_dependencies(icrm_class, methods)
    
    # Generate class name
    class_name = icrm_class.__name__
    crm_class_name = class_name[1:] if class_name.startswith('I') else f'{class_name}Impl'
    
    # Generate template content
    template_content = _generate_template_content_ast(
        icrm_class.__name__,
        crm_class_name,
        methods,
        icrm_class.__module__,
        transferable_deps
    )
    
    # Write to file
    output_path = Path(output_path) if isinstance(output_path, str) else output_path
    output_path.parent.mkdir(parents=True, exist_ok=True)
    
    with open(output_path, 'w') as f:
        f.write(template_content)

    logger.info(f'CRM template generated successfully at {output_path}')

def _format_arguments(args: ast.arguments) -> str:
    """Format function arguments from AST."""
    formatted_args = []
    
    # Handle regular arguments
    for i, arg in enumerate(args.args):
        if arg.arg == 'self':
            continue
        
        arg_str = arg.arg
        
        # Add type annotation
        if arg.annotation:
            arg_str += f': {ast.unparse(arg.annotation)}'
        
        # Add default value if exists
        defaults_start = len(args.args) - len(args.defaults)
        if i >= defaults_start:
            default_idx = i - defaults_start
            default_value = ast.unparse(args.defaults[default_idx])
            arg_str += f' = {default_value}'
        formatted_args.append(arg_str)
    
    # Handle *args
    if args.vararg:
        vararg_str = f'*{args.vararg.arg}'
        if args.vararg.annotation:
            vararg_str += f': {ast.unparse(args.vararg.annotation)}'
        formatted_args.append(vararg_str)
    
    # Handle **kwargs
    if args.kwarg:
        kwarg_str = f'**{args.kwarg.arg}'
        if args.kwarg.annotation:
            kwarg_str += f': {ast.unparse(args.kwarg.annotation)}'
        formatted_args.append(kwarg_str)

    return ', '.join(formatted_args)

def _format_return_annotation(returns: ast.expr) -> str:
    """Format return type annotation from AST."""
    if returns:
        return f' -> {ast.unparse(returns)}'
    return ''

def _extract_method_from_ast(class_node: ast.ClassDef) -> list[dict]:
    """Extract method information from AST class node."""
    methods = []
    
    for node in class_node.body:
        if isinstance(node, ast.FunctionDef) and not node.name.startswith('_') and not node.name.startswith('__'):
            # Extract method signature
            method_info = {
                'name': node.name,
                'args': _format_arguments(node.args),
                'returns': _format_return_annotation(node.returns),
                'docstring': ast.get_docstring(node) or '',
            }
            methods.append(method_info)
    return methods

def _extract_types_from_annotation(annotation) -> set[str]:
    """Extract type names from a type annotation."""
    types = set()
    
    if hasattr(annotation, '__name__'):
        types.add(annotation.__name__)
    
    # Handle generic types
    origin = get_origin(annotation)
    args = get_args(annotation)
    
    if origin is not None:
        if hasattr(origin, '__name__'):
            types.add(origin.__name__)
        
        # Recursively extract types from arguments
        for arg in args:
            types.update(_extract_types_from_annotation(arg))

    # Handle Union types, Optional, etc.
    if hasattr(annotation, '__args__'):
        for arg in annotation.__args__:
            if arg is not type(None): # Skip None type
                types.update(_extract_types_from_annotation(arg))
    
    return types

def _extract_types_from_ast_expr(expr: ast.expr) -> set[str]:
    """Extract type names from AST expression."""
    types = set()
    
    if isinstance(expr, ast.Name):
        types.add(expr.id)
    elif isinstance(expr, ast.Subscript):
        # Handle generic types like list[GridAttribute]
        types.update(_extract_types_from_ast_expr(expr.value))
        types.update(_extract_types_from_ast_expr(expr.slice))
    elif isinstance(expr, ast.Tuple):
        # Handle tuple types
        for elt in expr.elts:
            types.update(_extract_types_from_ast_expr(elt))
    elif isinstance(expr, ast.List):
        # Handle list in type annotations
        for elt in expr.elts:
            types.update(_extract_types_from_ast_expr(elt))
    elif isinstance(expr, ast.Attribute):
        # Handle module.Type references
        types.add(ast.unparse(expr))
    
    return types

def _extract_types_from_ast_node(node: ast.FunctionDef) -> set[str]:
    """Extract type names from AST function node."""
    types = set()
    
    # Extract from arguments
    for arg in node.args.args:
        if arg.annotation:
            types.update(_extract_types_from_ast_expr(arg.annotation))

    # Extract from return annotation
    if node.returns:
        types.update(_extract_types_from_ast_expr(node.returns))
    
    return types

def _extract_transferable_dependencies(icrm_class: Type[T], methods: list[dict]) -> set[str]:
    """Extract all transferable types used in method signatures."""
    transferable_deps = set()
    
    # Extract types from method signatures
    for method in methods:
        method_name = method['name']
        
        # Get method from class
        if hasattr(icrm_class, method_name):
            method_func = getattr(icrm_class, method_name)
            try:
                method_hints = get_type_hints(method_func)
                for hint_name, hint_type in method_hints.items():
                    transferable_deps.update(_extract_types_from_annotation(hint_type))
            except (NameError, AttributeError):
                # Parse from AST if runtime type hints fail
                ast_node = method['ast_node'] 
                transferable_deps.update(_extract_types_from_ast_node(ast_node))
    
    # Filter to only include types that are likely transferables
    # (type that are defined in the same module as the ICRM)
    icrm_module = sys.modules[icrm_class.__module__]
    filtered_deps = set()
    
    for dep in transferable_deps:
        if hasattr(icrm_module, dep):
            # Check if it's a transferable class
            dep_class = getattr(icrm_module, dep)
            if hasattr(dep_class, '__dict__') and hasattr(dep_class, 'serialize') and hasattr(dep_class, 'deserialize'):
                filtered_deps.add(dep)
    
    return filtered_deps

def _generate_method_template_ast(method_info: dict) -> list[str]:
    """Generate template for a single method using AST information."""
    name = method_info['name']
    args = method_info['args']
    returns = method_info['returns']
    docstring = method_info['docstring']
    
    # Build method signature
    args_str = f', {args}' if args else ''
    method_signature = f'    def {name}(self{args_str}){returns}:'
    method_lines = [method_signature]
    
    # Add docstring if available
    if docstring:
        # Clean up the docstring and format it properly
        docstring_lines = docstring.strip().split('\n')
        method_lines.append('        """')
        for line in docstring_lines:
            method_lines.append(f'        {line}')
        method_lines.append('        """')
        
    else:
        method_lines.append(f'        """TODO: Implement {name} method."""')
    
    # Add todo comment and placeholder
    method_lines.extend([
        f'        # Implement {name} logic here',
        '        ...'
    ])
    
    return method_lines

def _generate_template_content_ast(icrm_name: str, crm_name: str, methods: list, icrm_module: str, transferable_deps: set[str]) -> str:
    """Generate the template file content using AST-extracted information."""
    
    # Build import statements
    import_lines = ['import c_two as cc']
    
    # Import ICRM
    icrm_import = f'from {icrm_module} import {icrm_name}'
    
    # Import transferable dependencies
    if transferable_deps:
        transferable_import = f'from {icrm_module} import {", ".join(sorted(transferable_deps))}'
        import_lines.extend([icrm_import, transferable_import])
    else:
        import_lines.append(icrm_import)
        
    # Start building the template
    lines = import_lines + [
        '',
        '@cc.iicrm',
        f'class {crm_name}({icrm_name}):',
        '    """',
        '    This is an auto-generated template. Please implement the methods below.',
        '    """',
        ''
    ]
    
    # Add methods templates
    for method in methods:
        method_lines = _generate_method_template_ast(method)
        lines.extend(method_lines)
        lines.append('')
    
    return '\n'.join(lines)