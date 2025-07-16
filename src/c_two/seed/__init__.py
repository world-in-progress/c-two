import sys
import click

from .docker_builder import CRMImageBuilder

@click.argument('project_path', type=click.Path(exists=True))
@click.option('--base-image', '-b', required=True, help='Base Docker image (e.g., python:3.12-slim)')
@click.option('--image', '-i', default=None, help='Docker image name (e.g., my-registry/hello-crm:v1.0)')
@click.option('--push', '-p', is_flag=True, help='Push image to registry after build')
@click.option('--registry', '-r', help='Docker registry URL')
def build(project_path: str, image: str | None, base_image: str, push: bool, registry: str | None):
    """Build a Docker image for CRM project deployment"""
    try:
        builder = CRMImageBuilder(project_path, base_image)

        # Build the Dockerfile
        builder.build_dockerfile()
        click.echo(f'Successfully built Dockerfile')
        
        if image:
            click.echo(f'Building Docker image: {image} with base image: {base_image}')
            image_id = builder.build_image(image)
        
            click.echo(f'Successfully built Docker image: {image}')
            click.echo(f'Image tag: {image_id}')
        
            if push:
                if not registry:
                    click.echo('Registry URL not provided, skipping push')
                else:
                    click.echo(f'Pushing Docker image to registry: {registry}')
                builder.push_image(image, registry)
                click.echo(f'Successfully pushed Docker image: {image} to registry: {registry}')

    except Exception as e:
        click.echo(f'Error building Docker image: {e}', err=True)
        sys.exit(1)