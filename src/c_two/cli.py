import click
import logging
from .seed import build
from . import __version__, LOGO_ASCII

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

@click.group()
@click.version_option(version=__version__, prog_name='C-Two')
def cli():
    """C-Two Command-Line Interface (cccli, c3)"""

cli.command(build)
print(LOGO_ASCII)