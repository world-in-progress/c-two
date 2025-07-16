from . import __version__

# C-Two Command-Line Interface (cccli)
import click
import logging

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

from .seed import build

@click.group()
@click.version_option(version=__version__, prog_name='C-Two')
def cli():
    """C-Two Command-Line Interface (c2)"""
    pass

cli.command(build)