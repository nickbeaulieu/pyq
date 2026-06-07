# A Django management command — a script. Its CLI args / env reads are its own
# and must NOT appear in the default app view; only when queried by name.
import os


class Command:
    def add_arguments(self, parser):
        parser.add_argument("--dry-run")
        parser.add_argument("start_date")

    def handle(self, *args, **opts):
        token = os.getenv("BACKFILL_TOKEN")
        return token
