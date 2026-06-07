def reverse_choices(x):
    return x


class Doc:
    @property
    def label(self):
        # A property getter that calls the target. Accessing `doc.label`
        # (no parens) reaches `reverse_choices` even though it's not a call site.
        return reverse_choices("fmt")
