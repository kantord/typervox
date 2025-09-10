import sys
import click

from .core import is_running, start, request_stop, status_message


@click.group()
def main():
    """Typervox voice typing CLI."""


@main.command("type")
@click.option("--model", default=None, help="Whisper model name (e.g., tiny, small, medium, large)")
def cmd_type(model: str | None):
    """Start voice typing (singleton)."""
    if is_running():
        click.echo("Already running: Voice typing", err=True)
        sys.exit(1)
    # Start blocks until stopped
    start(model)


@main.command("stop")
def cmd_stop():
    """Ask the running app to stop after 2s."""
    request_stop()


@main.command()
def status():
    """Prints current status; empty output if not recording."""
    msg = status_message()
    if msg:
        click.echo(msg)
