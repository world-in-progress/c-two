import os
import sys
import logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../..')))

import src.c_two as cc
from ifeature import IFeature

if __name__ == '__main__':
    
    with cc.compo.runtime.connect_crm('tcp://localhost:5555', IFeature) as feature:
        result = feature.upload_feature('test_path', 'json')
        logger.info(f'{result}')
        

