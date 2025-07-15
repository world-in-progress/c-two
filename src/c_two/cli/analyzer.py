import ast
from pathlib import Path

class CRMAnalyzer(ast.NodeVisitor):
    """AST分析器，用于检测CRM和ICRM类"""
    
    def __init__(self):
        self.crm_classes = []
        self.icrm_classes = []
        self.imports = set()
        self.current_decorators = []
    
    def visit_Import(self, node):
        for alias in node.names:
            self.imports.add(alias.name)
        self.generic_visit(node)
    
    def visit_ImportFrom(self, node):
        if node.module:
            self.imports.add(node.module)
        self.generic_visit(node)
    
    def visit_FunctionDef(self, node):
        # 检查装饰器
        for decorator in node.decorator_list:
            if isinstance(decorator, ast.Name):
                self.current_decorators.append(decorator.id)
            elif isinstance(decorator, ast.Attribute):
                self.current_decorators.append(f"{decorator.value.id}.{decorator.attr}")
        self.generic_visit(node)
    
    def visit_ClassDef(self, node):
        decorators = []
        for decorator in node.decorator_list:
            if isinstance(decorator, ast.Name):
                decorators.append(decorator.id)
            elif isinstance(decorator, ast.Attribute):
                decorators.append(f"{decorator.value.id}.{decorator.attr}")
        
        # 检测ICRM和CRM类
        if any(d in ['cc.icrm', 'icrm'] for d in decorators):
            self.icrm_classes.append(node.name)
        elif any(d in ['cc.iicrm', 'iicrm'] for d in decorators):
            self.crm_classes.append(node.name)
            
        self.generic_visit(node)
    
    
    def analyze_file(self, file_path: Path) -> dict[str, any]:
        with open(file_path, 'r', encoding='utf-8') as f:
            try:
                tree = ast.parse(f.read())
            except SyntaxError:
                return {'crm_classes': [], 'icrm_classes': [], 'imports': set()}
        
        self.visit(tree)
        
        return {
            'crm_classes': self.crm_classes,
            'icrm_classes': self.icrm_classes,
            'imports': self.imports
        }