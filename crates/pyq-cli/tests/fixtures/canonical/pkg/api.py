import functools

from rest_framework import serializers


class WidgetSerializer(serializers.ModelSerializer):
    # framework-driven: DRF instantiates and drives it, no test calls it
    # directly — must NOT be flagged untested-public.
    class Meta:
        fields = "__all__"


@functools.cache
def cached_handler(x):
    # decorated → framework/runtime entry — must NOT be flagged untested-public.
    return x
