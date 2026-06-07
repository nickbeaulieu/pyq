from abc import ABC


class Animal(ABC):
    def speak(self):
        ...

    def move(self):
        ...


class Dog(Animal):
    def speak(self):
        return "woof"


class Puppy(Dog):
    def speak(self):
        return "yip"
