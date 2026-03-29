import click
import logging
import re
import signal
import sys
import threading

from . import __version__, LOGO_ASCII
from .seed import build

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_SIZE_RE = re.compile(r'^(\d+(?:\.\d+)?)\s*(KB|MB|GB|B)?$', re.IGNORECASE)

_SIZE_UNITS: dict[str, int] = {
    'b': 1,
    'kb': 1024,
    'mb': 1024 ** 2,
    'gb': 1024 ** 3,
}


def parse_size(value: str) -> int:
    """Parse a human-readable size string into bytes.

    Supported formats: ``'256MB'``, ``'1GB'``, ``'128mb'``, ``'1024'`` (bytes).
    """
    m = _SIZE_RE.match(value.strip())
    if m is None:
        raise click.BadParameter(
            f'Invalid size format: {value!r}. '
            f'Expected <number>[KB|MB|GB], e.g. 256MB',
        )
    number = float(m.group(1))
    unit = (m.group(2) or 'b').lower()
    return int(number * _SIZE_UNITS[unit])


def parse_upstream(value: str) -> tuple[str, str]:
    """Parse ``'name=address'`` into ``(name, address)``."""
    if '=' not in value:
        raise click.BadParameter(
            f'Invalid upstream format: {value!r}. '
            f'Expected name=address, e.g. grid=ipc-v3://my_server',
        )
    name, address = value.split('=', 1)
    name = name.strip()
    address = address.strip()
    if not name:
        raise click.BadParameter('Upstream name cannot be empty.')
    if not address:
        raise click.BadParameter('Upstream address cannot be empty.')
    return name, address


# ---------------------------------------------------------------------------
# CLI group
# ---------------------------------------------------------------------------

@click.group()
@click.version_option(version=__version__, prog_name='C-Two')
def cli():
    """C-Two Command-Line Interface (cccli, c3)"""


cli.command(build)


# ---------------------------------------------------------------------------
# c3 relay
# ---------------------------------------------------------------------------

_RELAY_BANNER = """\
 ╔═════════════════════════════════════════════════════════╗
   C-Two Relay Server                   
   {url:<37s}                          
   Engine: {engine:<28s}               
   Workers: {workers:<27s}             
 ╚═════════════════════════════════════════════════════════╝

 Control endpoints:
   POST /_register      Register upstream CRM
   POST /_unregister    Unregister upstream CRM
   GET  /_routes        List registered routes
   GET  /health         Health check

 Data plane:
   POST /{{route}}/{{method}}   Relay call to CRM

 Press Ctrl+C to stop.
"""


@cli.command()
@click.option(
    '--bind', '-b',
    default='0.0.0.0:8080',
    show_default=True,
    help='HTTP listen address (host:port).',
)
@click.option(
    '--workers', '-w',
    default=32,
    show_default=True,
    type=int,
    help='Thread pool size for concurrent relay calls.',
)
@click.option(
    '--native/--python',
    default=True,
    show_default=True,
    help='Relay engine: Rust NativeRelay (default) or Python Relay.',
)
@click.option(
    '--upstream', '-u',
    multiple=True,
    metavar='NAME=ADDRESS',
    help='Pre-register upstream CRM (repeatable).',
)
@click.option(
    '--log-level', '-l',
    default='INFO',
    show_default=True,
    type=click.Choice(
        ['DEBUG', 'INFO', 'WARNING', 'ERROR'],
        case_sensitive=False,
    ),
    help='Logging level.',
)
@click.option(
    '--segment-size',
    default=None,
    metavar='SIZE',
    help='Buddy pool segment size, e.g. 128MB (default: 256MB).',
)
@click.option(
    '--max-segments',
    default=None,
    type=int,
    metavar='N',
    help='Max buddy pool segments (default: 4).',
)
def relay(
    bind: str,
    workers: int,
    native: bool,
    upstream: tuple[str, ...],
    log_level: str,
    segment_size: str | None,
    max_segments: int | None,
):
    """Start the C-Two HTTP relay server.

    Bridges HTTP requests to CRM resource processes over IPC v3.
    CRM processes auto-register via C2_RELAY_ADDRESS, or use --upstream
    to pre-register at startup.
    """
    logging.getLogger().setLevel(getattr(logging, log_level.upper()))

    # Parse upstream entries early to fail fast on bad format.
    parsed_upstreams: list[tuple[str, str]] = []
    for u in upstream:
        parsed_upstreams.append(parse_upstream(u))

    # Build IPC config if overrides provided.
    ipc_config = None
    if segment_size is not None or max_segments is not None:
        from .transport.ipc.frame import IPCConfig
        kwargs: dict = {}
        if segment_size is not None:
            kwargs['pool_segment_size'] = parse_size(segment_size)
        if max_segments is not None:
            kwargs['pool_max_segments'] = max_segments
        ipc_config = IPCConfig(**kwargs)

    # Create and start relay.
    if native:
        relay_instance = _start_native_relay(bind, parsed_upstreams)
        engine = 'Rust (NativeRelay)'
    else:
        relay_instance = _start_python_relay(
            bind, workers, ipc_config, parsed_upstreams,
        )
        engine = 'Python (Relay)'

    # Print banner.
    url = _relay_url(bind)
    click.echo(_RELAY_BANNER.format(
        url=url,
        engine=engine,
        workers=str(workers),
    ))

    # Block until signal.
    stop = threading.Event()

    def _handle_signal(signum, frame):  # noqa: ARG001
        stop.set()

    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    stop.wait()

    # Graceful shutdown.
    click.echo('\n Shutting down relay…')
    _stop_relay(relay_instance, native)
    click.echo(' Relay stopped.')


def _relay_url(bind: str) -> str:
    """Build a display URL from the bind string."""
    if ':' in bind:
        host, port = bind.rsplit(':', 1)
    else:
        host, port = bind, '8080'
    if host in ('0.0.0.0', '::'):
        host = '127.0.0.1'
    return f'http://{host}:{port}'


def _start_python_relay(
    bind: str,
    workers: int,
    ipc_config,
    upstreams: list[tuple[str, str]],
):
    from .transport.relay import Relay

    relay = Relay(bind=bind, max_workers=workers, ipc_config=ipc_config)
    relay.start(blocking=False)

    for name, address in upstreams:
        try:
            relay.upstream_pool.add(name, address)
        except Exception as exc:
            click.echo(f'Error: failed to register upstream {name!r}: {exc}', err=True)
            relay.stop()
            sys.exit(1)

    return relay


def _start_native_relay(
    bind: str,
    upstreams: list[tuple[str, str]],
):
    try:
        from .relay import NativeRelay
    except ImportError:
        click.echo(
            'Error: NativeRelay (Rust) not available. '
            'Build with `uv sync` or omit --native.',
            err=True,
        )
        sys.exit(1)

    relay = NativeRelay(bind)
    relay.start()

    for name, address in upstreams:
        try:
            relay.register_upstream(name, address)
        except Exception as exc:
            click.echo(f'Error: failed to register upstream {name!r}: {exc}', err=True)
            relay.stop()
            sys.exit(1)

    return relay


def _stop_relay(relay_instance, native: bool) -> None:
    relay_instance.stop()


print(LOGO_ASCII)