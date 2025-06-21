import ast
import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
import c_two as cc

if __name__ == '__main__':
    from icrm import IGrid

    icrm_ast = cc.crm.generate_crm_template(IGrid, 'tests/template/crm.template.py')
    