from models import Doc


def render(doc: Doc):
    # Property ACCESS, not a call — `doc.label` runs Doc.label's getter, which
    # reaches reverse_choices. The transpose of `outgoing` can't see this (the
    # getter is accessed, not called); ty's incoming view does.
    return doc.label
