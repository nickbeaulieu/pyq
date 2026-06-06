class Alpha:
    def process(self):
        return 1


class Beta:
    def process(self):
        return 2


def use(a: Alpha, b: Beta):
    a.process()
    b.process()
