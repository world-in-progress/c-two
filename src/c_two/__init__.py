from dotenv import load_dotenv
load_dotenv()

from . import rpc
from . import mcp
from . import compo
from . import error
from .compo import runtime
from .crm.meta import icrm
from .rpc.transferable import transferable

__version__ = '0.2.7'

LOGO_ASCII ="""
                                                  
                                                  
                       cccc                       
                   cccccccccccc                   
               ccc  ccccccccc    cc               
          cccc      ccccccccc        ccc          
      ccccccc       cccc cccc        ccccccc      
     cccccccc       cccc cccc        ccccccc      
     cccccccc       cccc cccc        ccccccc      
     ccccccc        cccc cccc        ccccccc      
     ccc            cccc cccc        ccc          
                    cccc cccc                     
                    cccc cccc                     
                    cccc cccc                     
           cc       cccc cccc              c      
       cccccc       cccc cccc         cccccc      
     cccccccc       cccc cccc        ccccccc      
     cccccccc       cccc cccc        ccccccc      
     cccccccc       cccc cccc        ccccccc      
         cccc       cccc cccc       cccc          
              cc    cccc cccc   cccc              
                  cccccc ccccccc                  
                      cc cc                       
                                                  
                                                  
"""