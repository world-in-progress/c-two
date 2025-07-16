import os
import ast
import docker
import hashlib
import logging
import subprocess
from pathlib import Path

logger = logging.getLogger(__name__)

def generate_md5(input_string):
    md5_hasher = hashlib.md5()
    md5_hasher.update(input_string.encode('utf-8'))
    md5_hash = md5_hasher.hexdigest()
    return md5_hash

class CRMImageBuilder:
    def __init__(self, project_path: str, base_image: str):
        self.base_image = base_image
        self.project_path = Path(project_path)
        self.docker_client = docker.from_env()
    
    def _detect_arg_library(self, tree: ast.AST) -> dict[str, any]:
        """Detect which argument parsing library is being used"""
        
        imports = {
            'argparse': False,
            'click': False,
            'typer': False,
            'fire': False
        }
        
        # Walk through AST to find import statements
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                for alias in node.names:
                    if alias.name in imports:
                        imports[alias.name] = True
            elif isinstance(node, ast.ImportFrom):
                if node.module in imports:
                    imports[node.module] = True
        
        # Determine primary library (priority order)
        for lib in ['click', 'typer', 'argparse', 'fire']:
            if imports[lib]:
                return {'library': lib, 'detected_imports': imports}
        
        return {'library': None, 'detected_imports': imports}

    def _parse_argparse_add_argument(self, node: ast.Call) -> dict[str, any]:
        """Parse argparse add_argument call to extract argument information"""
        
        arg_info = {
            'name': None,
            'flags': [],
            'required': False,
            'type': None,
            'default': None,
            'help': None,
            'choices': None,
            'nargs': None,
            'action': None,
            'dest': None
        }
        
        # Extract positional arguments (argument names/flags)
        for arg in node.args:
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                flag = arg.value
                arg_info['flags'].append(flag)
                
                # Determine if it's a positional argument (no dashes) or optional (with dashes)
                if not flag.startswith('-'):
                    arg_info['name'] = flag
                    arg_info['required'] = True
                else:
                    # For optional arguments, use the long form as name if available
                    if flag.startswith('--'):
                        arg_info['name'] = flag[2:]
                    elif not arg_info['name']:  # only set if no long form found yet
                        arg_info['name'] = flag[1:]
        
        # Extract keyword arguments
        for keyword in node.keywords:
            key = keyword.arg
            value = keyword.value
            
            if key == 'required':
                # Handle required parameter
                if isinstance(value, ast.Constant):
                    arg_info['required'] = bool(value.value)
            
            elif key == 'type':
                # Handle type parameter
                if isinstance(value, ast.Name):
                    arg_info['type'] = value.id
                elif isinstance(value, ast.Attribute):
                    # Handle cases like int, str, etc.
                    if isinstance(value.value, ast.Name):
                        arg_info['type'] = f"{value.value.id}.{value.attr}"
            
            elif key == 'default':
                # Handle default value
                if isinstance(value, ast.Constant):
                    arg_info['default'] = value.value
                elif isinstance(value, ast.Name):
                    arg_info['default'] = value.id
            
            elif key == 'help':
                # Handle help text
                if isinstance(value, ast.Constant) and isinstance(value.value, str):
                    arg_info['help'] = value.value
            
            elif key == 'choices':
                # Handle choices parameter
                if isinstance(value, ast.List):
                    choices = []
                    for item in value.elts:
                        if isinstance(item, ast.Constant):
                            choices.append(item.value)
                    arg_info['choices'] = choices
            
            elif key == 'nargs':
                # Handle nargs parameter
                if isinstance(value, ast.Constant):
                    arg_info['nargs'] = value.value
            
            elif key == 'action':
                # Handle action parameter
                if isinstance(value, ast.Constant) and isinstance(value.value, str):
                    arg_info['action'] = value.value
            
            elif key == 'dest':
                # Handle dest parameter
                if isinstance(value, ast.Constant) and isinstance(value.value, str):
                    arg_info['dest'] = value.value
                    # Use dest as name if no name was determined from flags
                    if not arg_info['name']:
                        arg_info['name'] = value.value
        
        # If no name was determined, use the first flag without dashes
        if not arg_info['name'] and arg_info['flags']:
            first_flag = arg_info['flags'][0]
            if first_flag.startswith('--'):
                arg_info['name'] = first_flag[2:]
            elif first_flag.startswith('-'):
                arg_info['name'] = first_flag[1:]
            else:
                arg_info['name'] = first_flag
        
        # Special handling for store_true/store_false actions
        if arg_info['action'] in ['store_true', 'store_false']:
            arg_info['required'] = False
            arg_info['type'] = 'bool'
            arg_info['default'] = False if arg_info['action'] == 'store_true' else True
        
        return arg_info

    def _extract_argparse_args(self, tree: ast.AST, library_info: dict) -> dict[str, any]:
        """Extract argument information from argparse usage"""
        
        args_info = {
            'required_args': [],
            'optional_args': [],
            'subcommands': [],
            'help_available': True
        }
        
        # Look for ArgumentParser instantiation and add_argument calls
        for node in ast.walk(tree):
            if isinstance(node, ast.Call):
                # Check for add_argument calls
                if (isinstance(node.func, ast.Attribute) and 
                    node.func.attr == 'add_argument'):
                    
                    arg_info = self._parse_argparse_add_argument(node)
                    if arg_info['required']:
                        args_info['required_args'].append(arg_info)
                    else:
                        args_info['optional_args'].append(arg_info)
        
        return args_info

    def _extract_click_args(self, tree: ast.AST, library_info: dict) -> dict[str, any]:
        """Extract argument information from click decorators"""
        
        args_info = {
            'required_args': [],
            'optional_args': [],
            'subcommands': [],
            'help_available': True
        }
        
        # Look for click decorators on functions
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef):
                for decorator in node.decorator_list:
                    if isinstance(decorator, ast.Call):
                        # Check for click.option, click.argument, etc.
                        if (isinstance(decorator.func, ast.Attribute) and
                            isinstance(decorator.func.value, ast.Name) and
                            decorator.func.value.id == 'click'):
                            
                            arg_info = self._parse_click_decorator(decorator)
                            if arg_info:
                                if arg_info['required']:
                                    args_info['required_args'].append(arg_info)
                                else:
                                    args_info['optional_args'].append(arg_info)
        
        return args_info
    
    def _analyze_main_py_args(self) -> dict[str, any]:
        main_py_path = self.project_path / 'main.py'
        
        # Initialize result structure
        result = {
            'library': None,
            'has_args': False,
            'required_args': [],
            'optional_args': [],
            'subcommands': [],
            'help_available': False,
            'parsing_errors': []
        }
        
        try:
            # Read and parse the main.py file
            with open(main_py_path, 'r', encoding='utf-8') as f:
                content = f.read()
            
            # Parse the content to an AST
            tree = ast.parse(content)
            
            # Analyze imports to detect argument parsing libraries
            library_info = self._detect_arg_library(tree)
            result['library'] = library_info['library']
            
            # Extract argument definitions based on the detected library
            if library_info['library'] == 'argparse':
                args_info = self._extract_argparse_args(tree, library_info)
            elif library_info['library'] == 'click':
                args_info = self._extract_click_args(tree, library_info)
            else:
                raise ValueError(f'Unsupported argument parsing library: {library_info["library"]}')

            # Update result with extracted information
            result.update(args_info)
            result['has_args'] = bool(args_info['required_args'] or args_info['optional_args'])
        
        except SyntaxError as e:
            result['parsing_errors'].append(f"Syntax error in main.py: {e}")
        except FileNotFoundError:
            result['parsing_errors'].append("main.py file not found")
        except Exception as e:
            result['parsing_errors'].append(f"Error analyzing main.py: {e}")
        
        return result
    
    def _detect_environment(self) -> dict[str, any]:
        """Detect the project environment (uv, conda, poetry, pip)"""
        env_info = {'type': 'unknown', 'files': [], 'python_version': None}

        # Detect uv
        if (self.project_path / 'uv.lock').exists():
            env_info['type'] = 'uv'
            env_info['files'] = ['pyproject.toml', 'uv.lock']
            
        # Detect poetry
        elif (self.project_path / 'poetry.lock').exists():
            env_info['type'] = 'poetry'
            env_info['files'] = ['pyproject.toml', 'poetry.lock']

        # Detect conda
        elif (self.project_path / 'environment.yml').exists():
            env_info['type'] = 'conda'
            env_info['files'] = ['environment.yml']

        # Detect requirements.txt
        elif (self.project_path / 'requirements.txt').exists():
            env_info['type'] = 'pip'
            env_info['files'] = ['requirements.txt']
            
        return env_info
    
    def _detect_python_version(self, base_image: str) -> str:
        if base_image.startswith('python:'):
            return base_image.split(':')[1].split('-')[0]
        return '3.11'
    
    def _analyze_project(self) -> dict[str, any]:
        """Analyze the CRM project"""
        analysis = {
            'environment': self._detect_environment(),
            'python_version': self._detect_python_version(self.base_image),
            'main_py_args': self._analyze_main_py_args(),
        }
        
        return analysis
    
    def _generate_dockerignore(self) -> str:
        """Generate .dockerignore file"""
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
    
    def _generate_dockerfile(self, analysis: dict[str, any]) -> str:
        """Generate an optimized Dockerfile"""
        # Get proxy settings
        proxy_settings = {}
        if os.getenv('HTTP_PROXY'):
            http_proxy = os.getenv('HTTP_PROXY')
            address = http_proxy.split('//')[1]
            port = address.split(':')[1] if ':' in address else '80'
            proxy_settings['HTTP_PROXY'] = http_proxy if not address.startswith('127.0.0.1') else f'host.docker.internal:{port}'
        if os.getenv('HTTPS_PROXY'):
            https_proxy = os.getenv('HTTPS_PROXY')
            address = https_proxy.split('//')[1]
            port = address.split(':')[1] if ':' in address else '80'
            proxy_settings['HTTPS_PROXY'] = https_proxy if not address.startswith('127.0.0.1') else f'host.docker.internal:{port}'
        if os.getenv('NO_PROXY'):
            proxy_settings['NO_PROXY'] = os.getenv('NO_PROXY')
            
        env_type = analysis['environment']['type']
        python_version = analysis.get('python_version', '3.11')

        # Base image selection
        if env_type == 'conda':
            base_image = f'continuumio/miniconda3:latest'
        else:
            base_image = f'python:{python_version}-slim'
            
        dockerfile_parts = []
        
        # --------------------
        # -- Building Stage --
        # --------------------
        
        dockerfile_build_stage = [
            f'FROM {base_image} AS builder',
            ''
        ]

        # Add ARG and ENV for proxy settings if they exist
        if proxy_settings:
            dockerfile_build_stage.extend([
                '# Proxy settings for build time',
                'ARG HTTP_PROXY',
                'ARG HTTPS_PROXY',
                'ARG NO_PROXY',
            ])
            for key, value in proxy_settings.items():
                dockerfile_build_stage.append(f'ENV {key}="{value}"')
            dockerfile_build_stage.append('') # Add an empty line for readability
        
        dockerfile_build_stage.extend([
            '# Set working directory',
            'WORKDIR /app',
            '',
            '# Install system dependencies',
            'RUN apt-get update && apt-get install -y \\',
            '    gcc \\',
            '    g++ \\',
            '    curl \\',
            '    git \\',
            '    && rm -rf /var/lib/apt/lists/*',
            '',
        ])

        # Environment-specific installation steps
        if env_type == 'uv':
            dockerfile_build_stage.extend([
                '# Install curl and uv',
                'RUN curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=/usr/local/bin sh',
                '',
                '# Copy dependency files',
                'COPY pyproject.toml uv.lock ./',
                '',
                '# Install dependencies with verbose output',
                'RUN uv sync --frozen --verbose && \\',
                '    pip install c-two',
                '',
            ])
        elif env_type == 'poetry':
            dockerfile_build_stage.extend([
                '# Install Poetry',
                'RUN pip install poetry',
                'RUN poetry config virtualenvs.create false',
                '',
                '# Copy dependency files',
                'COPY pyproject.toml poetry.lock ./',
                '',
                '# Install dependencies',
                'RUN poetry install --no-dev',
                '',
            ])
        elif env_type == 'conda':
            dockerfile_build_stage.extend([
                '# Copy environment file',
                'COPY environment.yml ./',
                '',
                '# Create conda environment',
                'RUN conda env create -f environment.yml',
                "RUN echo 'conda activate $(head -1 environment.yml | cut -d' ' -f2)' >> ~/.bashrc",
                '',
            ])
        else:
            dockerfile_build_stage.extend([
                '# Copy requirements',
                'COPY requirements.txt ./',
                '',
                '# Install dependencies',
                'RUN pip install --no-cache-dir -r requirements.txt',
                '',
            ])
        
        dockerfile_parts.extend(dockerfile_build_stage)
        
        # -----------------
        # -- Final Stage --
        # -----------------
        dockerfile_final_stage = [
            f'FROM {base_image} AS final',
            '',
            '# Set working directory',
            'WORKDIR /app',
            '',
            'COPY --from=builder /app/.venv ./.venv',
            '',
            'ENV PATH="/app/.venv/bin:$PATH"',
            '',
            'COPY . .',
            '',
            'ENTRYPOINT ["python", "main.py"]'
        ]

        # Handle main.py arguments
        default_params = []
        main_py_args = analysis.get('main_py_args', {})
        if main_py_args and main_py_args.get('has_args', False):
            for arg in main_py_args.get('required_args', []):
                if arg.get('flags') and arg.get('default') is not None:
                    default_params.append(f"{arg['flags'][0]}={arg['default']}")
                    
            for arg in main_py_args.get('optional_args', []):
                if arg.get('flags') and arg.get('default') is not None:
                    default_params.append(f"{arg['flags'][0]}={arg['default']}")
        
        dockerfile_final_stage.append(f'CMD {default_params}')

        dockerfile_parts.extend(dockerfile_final_stage)

        return '\n'.join(dockerfile_parts)
    
    def build_dockerfile(self) -> None:
        # Check for main.py
        main_py = self.project_path / 'main.py'
        if not main_py.exists():
            raise FileNotFoundError(f'main.py not found in project root. A CRM project must have a main.py entrypoint.')
        
        # Detect environment and analyze project
        analysis = self._analyze_project()
        
        build_context = self.project_path

        # Generate Dockerfile
        dockerfile_content = self._generate_dockerfile(analysis)
        (build_context / 'Dockerfile').write_text(dockerfile_content)
        
        return str(build_context / 'Dockerfile')
        
    def build_image(self, image_tag: str) -> str:
        build_context = self.project_path

        # Generate .dockerignore
        dockerignore_content = self._generate_dockerignore()
        dockerignore_path = build_context / '.dockerignore'
        dockerignore_path.write_text(dockerignore_content)
        
        logger.info('-' * 50)
        logger.info(f'Starting Docker image build for tag: {image_tag}')
        logger.info(f'Build context: {build_context}')
        logger.info('-' * 50)
    
        command = [
            'docker', 'build',
            '--tag', image_tag,
            '--rm',
            '.',
        ]
        
        try:
            process = subprocess.Popen(
                command,
                cwd=build_context,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding='utf-8',
                bufsize=1
            )
            
            if process.stdout:
                for line in iter(process.stdout.readline, ''):
                    print(line.strip())
            
            # Wait for the process to complete
            return_code = process.wait()

            if return_code != 0:
                raise RuntimeError(f"Docker build failed: {return_code}")

            image = self.docker_client.images.get(image_tag)
            image_id = image.id
            
            return image_id

        except FileNotFoundError:
            logger.error("Error: 'docker' command not found. Please ensure Docker is installed and in your system's PATH.")
            raise RuntimeError("'docker' command not found.")
        except Exception as e:
            logger.error(f"Unknown error occurred during build: {e}")
            raise
        finally:
            if dockerignore_path.exists():
                dockerignore_path.unlink()
        
    def push_image(self, image_tag: str, registry: str | None = None):
        """Push image to registry"""
        if registry:
            # Tag for specific registry
            registry_tag = f'{registry}/{image_tag}'
            image = self.docker_client.images.get(image_tag)
            image.tag(registry_tag)
            image_tag = registry_tag
        
        # Push image
        self.docker_client.images.push(image_tag)