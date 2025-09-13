import os
import sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

import c_two as cc
from ihello import IHello

@cc.crm
class Hello(IHello):

    def greeting(self, name: str) -> str:
        return f'Hello, {name}!'
    