import sys
import json
import click
import tempfile
import subprocess
from pathlib import Path

try:
    import docker
except ImportError:
    docker = None

try:
    from .docker_builder import CRMDockerBuilder
except ImportError:
    CRMDockerBuilder = None

try:
    from .k8s_generator import K8sManifestGenerator
except ImportError:
    K8sManifestGenerator = None

@click.group()
def cli():
    """C-Two CRM Docker Seeding Tool"""
    pass

@cli.command()
@click.argument('crm_path', type=click.Path(exists=True))
@click.option('--image', '-i', required=True, help='Docker image name (e.g., my-registry/hello-crm:v1.0)')
@click.option('--base-image', '-b', default='python:3.11-slim', help='Base Docker image (e.g., python:3.12-slim)')
@click.option('--push', '-p', is_flag=True, help='Push image to registry after build')
@click.option('--registry', '-r', help='Docker registry URL')
def build(crm_path: str, image: str, base_image: str, push: bool, registry: str):
    """Build a Docker image for CRM deployment"""
    if docker is None:
        click.echo("Error: docker package is required for build command. Install with: pip install docker", err=True)
        sys.exit(1)
    
    if CRMDockerBuilder is None:
        click.echo("Error: CRMDockerBuilder is not available", err=True)
        sys.exit(1)
    
    try:
        builder = CRMDockerBuilder(crm_path, base_image)
        image_id = builder.build_image(image)
        
        if push:
            builder.push_image(image, registry)
        
        click.echo(f'Successfully built Docker image: {image}')
        click.echo(f'Image tag: {image_id}')
    except Exception as e:
        click.echo(f'Error building Docker image: {e}', err=True)
        sys.exit(1)

@cli.command()
@click.argument('crm_path', type=click.Path(exists=True))
@click.option('--namespace', default='default', help='Kubernetes namespace')
@click.option('--name', '-n', required=True, help='Deployment name')
@click.option('--image', '-i', required=True, help='Docker image name')
@click.option('--port', '-p', default=8000, help='Service port')
def deploy(crm_path: str, namespace: str, name: str, image: str, port: int):
    """Generate Kubernetes deployment manifests"""
    if K8sManifestGenerator is None:
        click.echo("Error: K8sManifestGenerator is not available", err=True)
        sys.exit(1)
    
    try:
        generator = K8sManifestGenerator(crm_path)
        manifests = generator.generate_manifests(name, image, port, 1, namespace)
        
        output_dir = Path('./k8s-manifests')
        output_dir.mkdir(exist_ok=True)
        
        for filename, content in manifests.items():
            with open(output_dir / filename, 'w') as f:
                f.write(content)
        
        click.echo(f"Kubernetes manifests generated in ./k8s-manifests/")
        click.echo(f"Generated files: {', '.join(manifests.keys())}")
    except Exception as e:
        click.echo(f'Error generating Kubernetes manifests: {e}', err=True)
        sys.exit(1)

@cli.command()
def version():
    """Show version information"""
    try:
        import c_two
        version = getattr(c_two, '__version__', 'unknown')
    except ImportError:
        version = 'unknown'
    
    click.echo(f"C-Two CRM Seeding Tool")
    click.echo(f"Version: {version}")

@cli.command()
@click.argument('crm_path', type=click.Path(exists=True))
def analyze(crm_path: str):
    """Analyze a CRM project and show detected components"""
    try:
        # Try to use full analyzer if available
        try:
            from .analyzer import CRMAnalyzer
            analyzer = CRMAnalyzer()
            results = analyzer.analyze_project(Path(crm_path))
            
            click.echo(f"Analysis results for: {crm_path}")
            click.echo(f"Environment type: {results.get('environment', {}).get('type', 'unknown')}")
            click.echo(f"ICRM classes: {', '.join(results.get('icrm_classes', []))}")
            click.echo(f"CRM classes: {', '.join(results.get('crm_classes', []))}")
            click.echo(f"Python files: {len(results.get('python_files', []))}")
            
        except ImportError:
            # Fallback to basic analysis
            click.echo("Analysis feature not available. Running basic check...")
            path = Path(crm_path)
            py_files = list(path.rglob('*.py'))
            click.echo(f"Found {len(py_files)} Python files in {crm_path}")
            
            env_files = []
            if (path / 'pyproject.toml').exists():
                env_files.append('pyproject.toml')
            if (path / 'uv.lock').exists():
                env_files.append('uv.lock')
            if (path / 'poetry.lock').exists():
                env_files.append('poetry.lock')
            if (path / 'requirements.txt').exists():
                env_files.append('requirements.txt')
            
            click.echo(f"Environment files: {', '.join(env_files) if env_files else 'none detected'}")
        
    except Exception as e:
        click.echo(f'Error analyzing project: {e}', err=True)
        sys.exit(1)