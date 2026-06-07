class Shape:
    def area(self):
        return 0


class Circle(Shape):
    def area(self):
        return _pi_helper()


def _pi_helper():
    return 3


def total(s: Shape):
    return s.area()


circle = Circle()
result = total(circle)
