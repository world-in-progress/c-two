import click
import logging
import re
import signal
import sys
import threading
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from PIL import Image

from . import __version__
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
            f'Expected name=address, e.g. grid=ipc://my_server',
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
   Idle timeout: {idle_timeout:<23s}             
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
    '--idle-timeout',
    default=300,
    show_default=True,
    type=int,
    metavar='SECONDS',
    help='Disconnect idle upstream IPC connections after this many seconds (0=disabled).',
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
    idle_timeout: int,
    segment_size: str | None,
    max_segments: int | None,
):
    """Start the C-Two HTTP relay server.

    Bridges HTTP requests to CRM resource processes over IPC.
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
        relay_instance = _start_native_relay(bind, parsed_upstreams, idle_timeout)
        engine = 'Rust (NativeRelay)'
    else:
        relay_instance = _start_python_relay(
            bind, workers, ipc_config, parsed_upstreams, idle_timeout,
        )
        engine = 'Python (Relay)'

    # Print banner.
    url = _relay_url(bind)
    idle_str = f'{idle_timeout}s' if idle_timeout > 0 else 'disabled'
    click.echo(_RELAY_BANNER.format(
        url=url,
        engine=engine,
        workers=str(workers),
        idle_timeout=idle_str,
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
    idle_timeout: int,
):
    from .transport.relay import Relay

    relay = Relay(bind=bind, max_workers=workers, ipc_config=ipc_config, idle_timeout=float(idle_timeout))
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
    idle_timeout: int,
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

    relay = NativeRelay(bind, idle_timeout_secs=idle_timeout)
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


# ---------------------------------------------------------------------------
# c3 dev  (developer tools — only available in development installs)
# ---------------------------------------------------------------------------

def _is_dev_environment() -> bool:
    """Check if we're running from a development checkout."""
    try:
        root = Path.cwd()
        for parent in [root, *root.parents]:
            if (parent / "pyproject.toml").exists() and (parent / "src" / "c_two").is_dir():
                return True
    except OSError:
        pass
    return False


if _is_dev_environment():

    @cli.group()
    def dev():
        """Developer tools for C-Two (only available in dev checkout)."""

    def _find_project_root() -> Path:
        """Walk up from CWD to find the project root (contains pyproject.toml)."""
        cwd = Path.cwd()
        for parent in [cwd, *cwd.parents]:
            if (parent / "pyproject.toml").exists():
                return parent
        raise click.ClickException(
            "Cannot find project root (no pyproject.toml). "
            "Run this command from inside the C-Two project tree."
        )

    def _image_to_halfblock(
        img: "Image.Image",
        width: int,
        threshold: int,
    ) -> str:
        """Convert a grayscale image to Unicode half-block art (2× vertical res)."""
        from PIL import Image as _Img

        aspect = img.height / img.width
        height = int(width * aspect) // 2 * 2  # must be even
        img = img.resize((width, height), _Img.Resampling.LANCZOS)

        lines: list[str] = []
        for y in range(0, height, 2):
            row: list[str] = []
            for x in range(width):
                top = img.getpixel((x, y)) > threshold
                bot = img.getpixel((x, y + 1)) > threshold if y + 1 < height else False
                if top and bot:
                    row.append("█")
                elif top:
                    row.append("▀")
                elif bot:
                    row.append("▄")
                else:
                    row.append(" ")
            lines.append("".join(row).rstrip())
        return "\n".join(lines)

    @dev.command("generate-banner")
    @click.option(
        "--width", "-w", default=50, show_default=True,
        help="Output width in characters.",
    )
    @click.option(
        "--threshold", "-t", default=100, show_default=True,
        help="Brightness threshold 0-255.",
    )
    @click.option(
        "--image", "-i", default=None, type=click.Path(exists=True),
        help="Source image path (default: docs/images/logo_bw.png).",
    )
    def generate_banner(
        width: int,
        threshold: int,
        image: str | None,
    ):
        """Generate Unicode half-block banner from the logo image.

        \b
        Output: src/c_two/banner_unicode.txt
        """
        try:
            from PIL import Image
        except ImportError:
            raise click.ClickException(
                "Pillow is required: uv pip install Pillow"
            )

        root = _find_project_root()
        pkg_dir = root / "src" / "c_two"
        bw_path = Path(image) if image else root / "docs" / "images" / "logo_bw.png"
        if not bw_path.exists():
            raise click.ClickException(f"Image not found: {bw_path}")

        img_gray = Image.open(bw_path).convert("L")
        art = _image_to_halfblock(img_gray, width=width, threshold=threshold)
        out = pkg_dir / "banner_unicode.txt"
        out.write_text(art + "\n", encoding="utf-8")
        click.echo(click.style(f"✓ {out.relative_to(root)}", fg="green")
                    + f"  ({len(art.splitlines())} lines)")
        click.echo(art)


# ---------------------------------------------------------------------------
# Banner on import
# ---------------------------------------------------------------------------

def _print_banner() -> None:
    """Print the C-Two logo banner if stdout is a TTY."""
    if not sys.stdout.isatty():
        return
    from . import LOGO_UNICODE
    click.echo(click.style(LOGO_UNICODE, fg=(102, 237, 173)))


_print_banner()