import os
import sys
import logging
import argparse
from pathlib import Path

# Import Feature (CRM)
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../..')))
import src.c_two as cc
from feature import Feature

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

if __name__ == '__main__':
    
    tcp_address = 'tcp://localhost:5555'
    
    # Init CRM
    crm = Feature(
        Path(os.path.join(os.path.dirname(__file__)))
    )
    
    # Launch CRM server
    logger.info('Starting CRM server...')
    server = cc.message.Server(tcp_address, crm)
    server.start()
    logger.info('CRM server started at %s', tcp_address)
    server.wait_for_termination()