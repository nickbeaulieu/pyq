from django.core.management.base import BaseCommand


class Command(BaseCommand):         # entrypoint file + framework class
    def handle(self, *args, **opts):
        return do_sync()


def do_sync():                      # live: reached from a management command
    return 1
