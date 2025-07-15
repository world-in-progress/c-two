import sys
import json
import click
import docker
import tempfile
import subprocess
from pathlib import Path

from .docker_builder import CRMDockerBuilder
from .k8s_generator import K8sManifestGenerator

@click.group()
def cli():
    """C-Two CRM Docker Seeding Tool"""
    pass

@cli.command()
@click.argument('crm_path', type=click.Path(exists=True))
@click.option('--image', '-i', required=True, help='Docker image name (e.g., my-registry/hello-crm:v1.0)')
@click.option('--base-image', '-b', required=True, help='Base Docker image (e.g., python:3.12-slim)')
@click.option('--push', '-p', is_flag=True, help='Push image to registry after build')
@click.option('--registry', '-r', help='Docker registry URL')
def build(crm_path: str, image: str, base_image: str, push: bool, registry: str):
    """Build a Docker image for CRM deployment"""
    builder = CRMDockerBuilder(crm_path, base_image)
    image_id = builder.build_image(image)
    
    if push:
        builder.push_image(image, registry)
    
    click.echo(f'Successfully built Docker image: {image}')
    click.echo(f'Image tag: {image_id}')

@cli.command()
@click.argument('crm_path', type=click.Path(exists=True))
@click.option('--namespace', default='default', help='Kubernetes namespace')
@click.option('--name', '-n', required=True, help='Deployment name')
@click.option('--image', '-i', required=True, help='Docker image name')
@click.option('--port', '-p', default=8000, help='Service port')
def deploy(crm_path: str, namespace: str, name: str, image: str, port: int):
    """Generate Kubernetes deployment manifests"""
    generator = K8sManifestGenerator(crm_path)
    manifests = generator.generate_manifests(name, image, port, 1, namespace)
    
    output_dir = Path('./k8s-manifests')
    output_dir.mkdir(exist_ok=True)
    
    for filename, content in manifests.items():
        with open(output_dir / filename, 'w') as f:
            f.write(content)
    
    click.echo(f'Kubernetes manifests generated in {output_dir}')