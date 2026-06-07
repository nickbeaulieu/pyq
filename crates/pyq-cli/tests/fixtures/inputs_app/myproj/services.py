# Ordinary app code — a config read here belongs to the app surface too.
WEBHOOK_URL = var_provider.get_var("WEBHOOK_URL")


def send(payload):
    return WEBHOOK_URL
