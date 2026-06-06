from rest_framework import serializers


class WidgetSerializer(serializers.ModelSerializer):   # framework class: live
    def get_label(self, obj):       # live: DRF SerializerMethodField convention
        return helper_for_label()

    class Meta:                     # live: inner config of a framework class
        fields = "__all__"


def helper_for_label():             # live: reached from a framework method
    return "x"
