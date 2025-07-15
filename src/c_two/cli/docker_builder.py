import docker
import shutil
import tempfile
from pathlib import Path

from .analyzer import CRMAnalyzer

class CRMDockerBuilder:
    def __init__(self, project_path: str, base_image: str = 'python:3.11-slim'):
        self.project_path = Path(project_path)
        self.base_image = base_image
        self.docker_client = docker.from_env()
        
    def build_image(self, image_tag: str) -> str:
        """构建Docker镜像"""
        with tempfile.TemporaryDirectory() as temp_dir:
            build_context = Path(temp_dir)
            
            # 分析项目
            analysis = self._analyze_project()
            
            # 准备构建上下文
            self._prepare_build_context(build_context, analysis)
            
            # 生成Dockerfile
            dockerfile_content = self._generate_dockerfile(analysis)
            (build_context / 'Dockerfile').write_text(dockerfile_content)
            
            # 构建镜像
            image, logs = self.docker_client.images.build(
                path=str(build_context),
                tag=image_tag,
                rm=True,
                forcerm=True
            )
            
            return image.id
    
    def _detect_python_version(self, base_image: str) -> str:
        if base_image.startswith('python:'):
            return base_image.split(':')[1].split('-')[0]
        return '3.11'
    
    def _detect_entry_points(self, analysis: dict[str, any]) -> list[dict[str, str]]:
        """检测CRM入口点"""
        entry_points = []
        
        if not analysis['crm_classes']:
            return entry_points
        
        # Re-scan files to get class-to-file mapping
        class_file_mapping = {}
        analyzer = CRMAnalyzer()
        
        for py_file in self.project_path.rglob('*.py'):
            if py_file.name.startswith('.'):
                continue
                
            file_analysis = analyzer.analyze_file(py_file)
            
            # Map each CRM class to its file
            for crm_class in file_analysis['crm_classes']:
                relative_path = py_file.relative_to(self.project_path)
                class_file_mapping[crm_class] = relative_path
        
        # Generate entry points for each CRM class
        for crm_class in analysis['crm_classes']:
            if crm_class not in class_file_mapping:
                continue
                
            file_path = class_file_mapping[crm_class]
            
            # Convert file path to module path
            module_path = str(file_path.with_suffix(''))
            module_path = module_path.replace('/', '.').replace('\\', '.')
            
            # Generate import statement
            import_statement = f"from {module_path} import {crm_class}"
            
            # Generate initialization code
            initialization_code = f"crm = {crm_class}()"
            
            entry_points.append({
                'class_name': crm_class,
                'import_statement': import_statement,
                'initialization_code': initialization_code,
                'file_path': str(file_path),
                'module_path': module_path
            })
        
        return entry_points
    
    def _analyze_project(self) -> dict[str, any]:
        """分析CRM项目"""
        analysis = {
            'environment': self._detect_environment(),
            'crm_classes': [],
            'icrm_classes': [],
            'entry_points': [],
            'dependencies': set(),
            'python_version': self._detect_python_version(self.base_image)
        }
        
        # 扫描Python文件找到CRM类
        analyzer = CRMAnalyzer()
        for py_file in self.project_path.rglob('*.py'):
            if py_file.name.startswith('.'):
                continue
            file_analysis = analyzer.analyze_file(py_file)
            analysis['crm_classes'].extend(file_analysis['crm_classes'])
            analysis['icrm_classes'].extend(file_analysis['icrm_classes'])
        
        # 自动检测入口点
        analysis['entry_points'] = self._detect_entry_points(analysis)
        
        return analysis
    
    
    def _detect_environment(self) -> dict[str, any]:
        """检测项目环境（uv、conda、poetry）"""
        env_info = {'type': 'unknown', 'files': [], 'python_version': None}
        
        # 检测uv
        if (self.project_path / 'uv.lock').exists():
            env_info['type'] = 'uv'
            env_info['files'] = ['pyproject.toml', 'uv.lock']
            
        # 检测poetry
        elif (self.project_path / 'poetry.lock').exists():
            env_info['type'] = 'poetry'
            env_info['files'] = ['pyproject.toml', 'poetry.lock']
            
        # 检测conda
        elif (self.project_path / 'environment.yml').exists():
            env_info['type'] = 'conda'
            env_info['files'] = ['environment.yml']
            
        # 检测requirements.txt
        elif (self.project_path / 'requirements.txt').exists():
            env_info['type'] = 'pip'
            env_info['files'] = ['requirements.txt']
            
        return env_info
    
    def _generate_dockerfile(self, analysis: dict[str, any]) -> str:
        """生成优化的Dockerfile"""
        env_type = analysis['environment']['type']
        python_version = analysis.get('python_version', '3.11')
        
        # 基础镜像选择
        if env_type == 'conda':
            base_image = f'continuumio/miniconda3:latest'
        else:
            base_image = f'python:{python_version}-slim'
        
        dockerfile_parts = [
            f"FROM {base_image}",
            "",
            "# Set working directory",
            "WORKDIR /app",
            "",
            "# Install system dependencies",
            "RUN apt-get update && apt-get install -y \\",
            "    gcc \\",
            "    g++ \\",
            "    && rm -rf /var/lib/apt/lists/*",
            "",
        ]
        
        # 环境特定的安装步骤
        if env_type == 'uv':
            dockerfile_parts.extend([
                "# Install uv with specific version",
                "RUN pip install uv>=0.1.0",
                "",
                "# Copy dependency files",
                "COPY pyproject.toml uv.lock ./",
                "",
                "# Install dependencies with verbose output",
                "RUN uv sync --frozen --verbose",
                "",
            ])
        elif env_type == 'poetry':
            dockerfile_parts.extend([
                "# Install Poetry",
                "RUN pip install poetry",
                "RUN poetry config virtualenvs.create false",
                "",
                "# Copy dependency files",
                "COPY pyproject.toml poetry.lock ./",
                "",
                "# Install dependencies",
                "RUN poetry install --no-dev",
                "",
            ])
        elif env_type == 'conda':
            dockerfile_parts.extend([
                "# Copy environment file",
                "COPY environment.yml ./",
                "",
                "# Create conda environment",
                "RUN conda env create -f environment.yml",
                "RUN echo 'conda activate $(head -1 environment.yml | cut -d' ' -f2)' >> ~/.bashrc",
                "",
            ])
        else:
            dockerfile_parts.extend([
                "# Copy requirements",
                "COPY requirements.txt ./",
                "",
                "# Install dependencies",
                "RUN pip install --no-cache-dir -r requirements.txt",
                "",
            ])
        
        # 复制应用代码
        dockerfile_parts.extend([
            "# Copy application code",
            "COPY . .",
            "",
            "# Install c-two if not in requirements",
            "RUN pip install c-two",
            "",
            "# Create entrypoint script",
            "COPY entrypoint.py ./",
            "",
            "# Expose port",
            "EXPOSE 8000",
            "",
            "# Health check",
            "HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \\",
            "    CMD python -c \"import requests; requests.get('http://localhost:8000/health', timeout=5)\" || exit 1",
            "",
            "# Run the application",
            "CMD [\"python\", \"entrypoint.py\"]"
        ])
        
        return '\n'.join(dockerfile_parts)

    def _prepare_build_context(self, build_context: Path, analysis: dict[str, any]):
        """准备Docker构建上下文"""
        # 复制项目文件
        shutil.copytree(
            self.project_path, 
            build_context / 'app',
            ignore=shutil.ignore_patterns('.git', '__pycache__', '*.pyc', '.pytest_cache')
        )
        
        # 移动文件到根目录
        for item in (build_context / 'app').iterdir():
            shutil.move(str(item), str(build_context / item.name))
        (build_context / 'app').rmdir()
        
        # 生成entrypoint.py
        entrypoint_content = self._generate_entrypoint(analysis)
        (build_context / 'entrypoint.py').write_text(entrypoint_content)
        
        # 生成.dockerignore
        dockerignore_content = self._generate_dockerignore()
        (build_context / '.dockerignore').write_text(dockerignore_content)
    
    def _generate_entrypoint(self, analysis: dict[str, any]) -> str:
        """生成Docker entrypoint脚本"""
        entry_points = analysis.get('entry_points', [])
        if not entry_points:
            raise ValueError("No CRM entry points detected")
        
        # 使用第一个检测到的CRM类
        primary_crm = entry_points[0]
        
        return f'''#!/usr/bin/env python3
"""
Auto-generated CRM Docker entrypoint
Generated by C-Two CRM Seeding Tool
"""
import os
import sys
import signal
import logging
from pathlib import Path

# Setup logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger(__name__)

def signal_handler(signum, frame):
    logger.info(f"Received signal {{signum}}, shutting down gracefully...")
    sys.exit(0)

def main():
    # Register signal handlers for graceful shutdown
    signal.signal(signal.SIGTERM, signal_handler)
    signal.signal(signal.SIGINT, signal_handler)
    
    # Get configuration from environment variables
    bind_address = os.getenv('CRM_BIND_ADDRESS', 'http://0.0.0.0:8000')
    crm_name = os.getenv('CRM_NAME', '{primary_crm['class_name']}')
    
    logger.info(f"Starting CRM: {{crm_name}}")
    logger.info(f"Bind address: {{bind_address}}")
    
    try:
        # Import the CRM class
        {primary_crm['import_statement']}
        
        # Initialize CRM with environment variables or defaults
        {primary_crm['initialization_code']}
        
        # Create and start server
        import c_two as cc
        server = cc.rpc.Server(bind_address, crm, name=crm_name)
        
        logger.info("Starting CRM server...")
        server.start()
        
        # Add health check endpoint for HTTP servers
        if bind_address.startswith('http://'):
            import threading
            import time
            from http.server import HTTPServer, BaseHTTPRequestHandler
            
            class HealthHandler(BaseHTTPRequestHandler):
                def do_GET(self):
                    if self.path == '/health':
                        self.send_response(200)
                        self.send_header('Content-type', 'application/json')
                        self.end_headers()
                        self.wfile.write(b'{{"status": "healthy", "service": "' + crm_name.encode() + b'"}}')
                    else:
                        self.send_response(404)
                        self.end_headers()
                        
                def log_message(self, format, *args):
                    pass  # Suppress default logging
            
            # Start health check server on a different port
            health_server = HTTPServer(('0.0.0.0', 8001), HealthHandler)
            health_thread = threading.Thread(target=health_server.serve_forever)
            health_thread.daemon = True
            health_thread.start()
            logger.info("Health check server started on port 8001")
        
        logger.info("CRM server started successfully")
        
        # Wait for termination
        try:
            server.wait_for_termination()
        except KeyboardInterrupt:
            logger.info("Received interrupt signal")
        finally:
            logger.info("Stopping CRM server...")
            server.stop()
            logger.info("CRM server stopped")
            
    except Exception as e:
        logger.error(f"Failed to start CRM: {{e}}")
        sys.exit(1)

if __name__ == '__main__':
    main()
'''
    
    def _generate_dockerignore(self) -> str:
        """生成.dockerignore文件"""
        return '''
.git
.gitignore
README.md
.pytest_cache
.coverage
.tox
.env
.venv
env/
venv/
ENV/
env.bak/
venv.bak/
__pycache__/
*.py[cod]
*$py.class
*.so
.Python
build/
develop-eggs/
dist/
downloads/
eggs/
.eggs/
lib/
lib64/
parts/
sdist/
var/
wheels/
*.egg-info/
.installed.cfg
*.egg
MANIFEST
*.log
.DS_Store
Thumbs.db
'''

    def push_image(self, image_tag: str, registry: str | None = None):
        """推送镜像到registry"""
        if registry:
            # Tag for specific registry
            registry_tag = f"{registry}/{image_tag}"
            image = self.docker_client.images.get(image_tag)
            image.tag(registry_tag)
            image_tag = registry_tag
        
        # Push image
        self.docker_client.images.push(image_tag)