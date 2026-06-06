import argparse
import click
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    db_url: str
    port: int = 5432
    debug = False  # not annotated -> not a field


def build_parser():
    parser = argparse.ArgumentParser()
    parser.add_argument("--verbose")
    parser.add_argument("path")
    return parser


@click.option("--count")
@click.argument("name")
def run(count, name):
    pass
