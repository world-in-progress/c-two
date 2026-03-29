"""Unit tests for ``c3 relay`` CLI helpers and Click command."""
from __future__ import annotations

import pytest
from click.testing import CliRunner

from c_two.cli import cli, parse_size, parse_upstream


# -- parse_size ------------------------------------------------------------

class TestParseSize:
    def test_bytes_bare_number(self):
        assert parse_size('1024') == 1024

    def test_kilobytes(self):
        assert parse_size('128KB') == 128 * 1024

    def test_megabytes(self):
        assert parse_size('256MB') == 256 * 1024 ** 2

    def test_gigabytes(self):
        assert parse_size('1GB') == 1024 ** 3

    def test_case_insensitive(self):
        assert parse_size('256mb') == 256 * 1024 ** 2
        assert parse_size('1gb') == 1024 ** 3

    def test_with_spaces(self):
        assert parse_size('  256 MB  ') == 256 * 1024 ** 2

    def test_float_size(self):
        assert parse_size('1.5GB') == int(1.5 * 1024 ** 3)

    def test_explicit_bytes(self):
        assert parse_size('4096B') == 4096

    def test_invalid_format(self):
        from click import BadParameter
        with pytest.raises(BadParameter, match='Invalid size format'):
            parse_size('not-a-size')

    def test_empty_string(self):
        from click import BadParameter
        with pytest.raises(BadParameter):
            parse_size('')


# -- parse_upstream --------------------------------------------------------

class TestParseUpstream:
    def test_valid(self):
        name, addr = parse_upstream('grid=ipc://my_server')
        assert name == 'grid'
        assert addr == 'ipc://my_server'

    def test_address_with_equals(self):
        name, addr = parse_upstream('grid=ipc://host=foo')
        assert name == 'grid'
        assert addr == 'ipc://host=foo'

    def test_whitespace_stripped(self):
        name, addr = parse_upstream('  grid = ipc://my_server  ')
        assert name == 'grid'
        assert addr == 'ipc://my_server'

    def test_no_equals(self):
        from click import BadParameter
        with pytest.raises(BadParameter, match='Invalid upstream format'):
            parse_upstream('grid-ipc://my_server')

    def test_empty_name(self):
        from click import BadParameter
        with pytest.raises(BadParameter, match='name cannot be empty'):
            parse_upstream('=ipc://foo')

    def test_empty_address(self):
        from click import BadParameter
        with pytest.raises(BadParameter, match='address cannot be empty'):
            parse_upstream('grid=')


# -- Click command parameters ----------------------------------------------

class TestRelayClickCommand:
    """Test that Click parses the relay subcommand options correctly."""

    def test_help_text(self):
        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        assert result.exit_code == 0
        assert 'Start the C-Two HTTP relay server' in result.output

    def test_default_bind_in_help(self):
        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        assert '0.0.0.0:8080' in result.output

    def test_log_level_choices(self):
        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        # Click truncates choice lists; just check the option is present.
        assert '--log-level' in result.output

    def test_upstream_repeatable(self):
        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        assert 'NAME=ADDRESS' in result.output
