import click
import logging
import re
import signal
import sys

# Reuse the registry's proxy-bypass-aware urllib opener so
# CLI admin commands honor the same C2_RELAY_USE_PROXY policy as runtime.
from c_two.transport.registry import _relay_urlopen
import threading
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from PIL import Image

from . import __version__

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
   GET  /_resolve/{{name}} Resolve resource name
   GET  /_peers          List known peers
   GET  /health         Health check

 Data plane:
   POST /{{route}}/{{method}}   Relay call to CRM

 Press Ctrl+C to stop.
"""


@cli.command()
@click.option(
    '--bind', '-b',
    default=None,
    help='HTTP listen address (host:port).  [env: C2_RELAY_BIND; default: 0.0.0.0:8080]',
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
    default=None,
    type=int,
    metavar='SECONDS',
    help='Disconnect idle upstream IPC connections after this many seconds (0=disabled).  [env: C2_RELAY_IDLE_TIMEOUT; default: 300]',
)
@click.option(
    '--seeds', '-s',
    default=None,
    help='Comma-separated seed relay URLs for mesh mode.  [env: C2_RELAY_SEEDS]',
)
@click.option(
    '--relay-id',
    default=None,
    help='Stable relay identifier for mesh protocol.  [env: C2_RELAY_ID]',
)
@click.option(
    '--advertise-url',
    default=None,
    help='Publicly reachable URL for this relay (announced to peers).  [env: C2_RELAY_ADVERTISE_URL]',
)
def relay(
    bind: str | None,
    upstream: tuple[str, ...],
    log_level: str,
    idle_timeout: int | None,
    seeds: str | None,
    relay_id: str | None,
    advertise_url: str | None,
):
    """Start the C-Two HTTP relay server.

    Bridges HTTP requests to CRM resource processes over IPC.
    CRM processes auto-register via C2_RELAY_ADDRESS, or use --upstream
    to pre-register at startup.
    """
    from .config.settings import settings

    # Resolve: CLI flag > env/.env (via settings) > hardcoded default
    bind = bind or settings.relay_bind or '0.0.0.0:8080'
    idle_timeout = idle_timeout if idle_timeout is not None else (settings.relay_idle_timeout or 300)
    seeds_str = seeds if seeds is not None else (settings.relay_seeds or '')
    relay_id = relay_id or settings.relay_id or ''
    advertise_url = advertise_url or settings.relay_advertise_url or ''
    logging.getLogger().setLevel(getattr(logging, log_level.upper()))

    # Parse upstream entries early to fail fast on bad format.
    parsed_upstreams: list[tuple[str, str]] = []
    for u in upstream:
        parsed_upstreams.append(parse_upstream(u))

    # Create and start relay.
    try:
        from .relay import NativeRelay
    except ImportError:
        click.echo(
            'Error: NativeRelay (Rust) not available. '
            'Build with `uv sync`.',
            err=True,
        )
        sys.exit(1)

    seed_list = [s.strip() for s in seeds_str.split(",") if s.strip()] if seeds_str else []
    relay_instance = NativeRelay(
        bind,
        relay_id=relay_id or None,
        advertise_url=advertise_url or None,
        seeds=seed_list if seed_list else None,
        idle_timeout=idle_timeout,
    )
    relay_instance.start()

    for name, address in parsed_upstreams:
        try:
            relay_instance.register_upstream(name, address)
        except Exception as exc:
            click.echo(f'Error: failed to register upstream {name!r}: {exc}', err=True)
            relay_instance.stop()
            sys.exit(1)

    # Print banner.
    url = _relay_url(bind)
    idle_str = f'{idle_timeout}s' if idle_timeout > 0 else 'disabled'
    click.echo(_RELAY_BANNER.format(
        url=url,
        engine='Rust (NativeRelay)',
        workers='-',
        idle_timeout=idle_str,
    ))

    # Show mesh info in banner if configured
    if seed_list:
        click.echo(f'  Mesh mode: {len(seed_list)} seed(s)')
        for s in seed_list:
            click.echo(f'    → {s}')
    if relay_id:
        click.echo(f'  Relay ID: {relay_id}')

    # Block until signal.
    stop = threading.Event()

    def _handle_signal(signum, frame):  # noqa: ARG001
        stop.set()

    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    stop.wait()

    # Graceful shutdown.
    click.echo('\n Shutting down relay…')
    relay_instance.stop()
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


# ---------------------------------------------------------------------------
# c3 registry  (resource registry commands)
# ---------------------------------------------------------------------------

@cli.group()
def registry():
    """Resource registry commands — query relay for routes and peers."""


@registry.command('list-routes')
@click.option('--relay', '-r', required=True, help='Relay HTTP address (e.g. http://localhost:8080)')
def registry_list_routes(relay):
    """List all registered routes on a relay."""
    import json
    import urllib.request

    url = f'{relay.rstrip("/")}/_routes'
    try:
        with _relay_urlopen(urllib.request.Request(url), timeout=5) as resp:
            routes = json.loads(resp.read())
            if not routes:
                click.echo('No routes registered.')
                return
            for name in routes:
                click.echo(name)
    except Exception as e:
        click.echo(f'Error: {e}', err=True)
        sys.exit(1)


@registry.command('resolve')
@click.option('--relay', '-r', required=True, help='Relay HTTP address')
@click.argument('name')
def registry_resolve(relay, name):
    """Resolve a resource name to route info."""
    import json
    import urllib.error
    import urllib.request

    url = f'{relay.rstrip("/")}/_resolve/{name}'
    try:
        with _relay_urlopen(urllib.request.Request(url), timeout=5) as resp:
            routes = json.loads(resp.read())
            for route in routes:
                click.echo(json.dumps(route, indent=2))
    except urllib.error.HTTPError as e:
        if e.code == 404:
            click.echo(f"Resource '{name}' not found", err=True)
        else:
            click.echo(f'Error: HTTP {e.code}', err=True)
        sys.exit(1)
    except Exception as e:
        click.echo(f'Error: {e}', err=True)
        sys.exit(1)


@registry.command('peers')
@click.option('--relay', '-r', required=True, help='Relay HTTP address')
def registry_peers(relay):
    """List known peer relays."""
    import json
    import urllib.request

    url = f'{relay.rstrip("/")}/_peers'
    try:
        with _relay_urlopen(urllib.request.Request(url), timeout=5) as resp:
            peer_list = json.loads(resp.read())
            if not peer_list:
                click.echo('No peers known.')
                return
            for peer in peer_list:
                status = peer.get('status', 'unknown')
                peer_id = peer.get('relay_id', '?')
                peer_url = peer.get('url', '?')
                click.echo(f'{peer_id} ({peer_url}) - {status}')
    except Exception as e:
        click.echo(f'Error: {e}', err=True)
        sys.exit(1)


# ---------------------------------------------------------------------------
# c3 dev  (developer tools — only available in development installs)
# ---------------------------------------------------------------------------

def _is_dev_environment() -> bool:
    """Check if we're running from a development checkout."""
    try:
        root = Path.cwd()
        for parent in [root, *root.parents]:
            if (
                (parent / "pyproject.toml").exists()
                and (parent / "sdk" / "python" / "src" / "c_two").is_dir()
            ):
                return True
    except OSError:
        pass
    return False


if _is_dev_environment():

    @cli.group()
    def dev():
        """Developer tools for C-Two (only available in dev checkout)."""

    def _find_project_root() -> Path:
        """Walk up from CWD to find the monorepo root."""
        cwd = Path.cwd()
        for parent in [cwd, *cwd.parents]:
            if (
                (parent / "pyproject.toml").exists()
                and (parent / "sdk" / "python" / "src" / "c_two").is_dir()
            ):
                return parent
        raise click.ClickException(
            "Cannot find project root. "
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
        Output: sdk/python/src/c_two/banner_unicode.txt
        """
        try:
            from PIL import Image
        except ImportError:
            raise click.ClickException(
                "Pillow is required: uv pip install Pillow"
            )

        root = _find_project_root()
        pkg_dir = root / "sdk" / "python" / "src" / "c_two"
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
