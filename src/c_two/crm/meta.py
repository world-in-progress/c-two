class ICRMMeta(type):
    """
    ICRMMeta
    --
    A metaclass for ICRM (Interface of Core Resource Model) classes that automatically sets a 'direction' attribute on new classes.
    Direction rules:
    - If a class explicitly defines its own 'direction', that value is preserved
    - If a class inherits from an ICRM class with direction '->', it gets assigned the direction '<-', meaning it is an implementation (CRM) of the ICRM  interface
    - Otherwise, it defaults to direction '->' (becoming a base ICRM class)
    
    This metaclass helps distinguish between ICRM interfaces ('->' direction) and their implementations (CRM, '<-' direction).
    """
    
    def __new__(mcs, name, bases, attrs, **kwargs):
        
        if 'direction' in attrs:
            return super().__new__(mcs, name, bases, attrs, **kwargs)
        
        has_icrm_parent = False
        for base in bases:
            if hasattr(base, 'direction') and base.direction == '->':
                attrs['direction'] = '<-'
                has_icrm_parent = True
                break
        
        if not has_icrm_parent:
            attrs['direction'] = '->'
        
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        return cls
    
def icrm(cls):
    """
    Convert a regular class to an ICRM class with direction '->'.
    
    This function transforms the given class by applying the ICRMMeta metaclass,
    making it a proper ICRM that other classes can implement.
    """
    attrs = {name: value for name, value in cls.__dict__.items() 
             if name not in ('__dict__', '__weakref__')}
    
    return ICRMMeta(cls.__name__, cls.__bases__, attrs)
